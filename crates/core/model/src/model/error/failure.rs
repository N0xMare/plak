use super::{
    AttemptId, ContextAdmissionError, ContextCapacityFailure, Deserialize, Deserializer, Digest,
    MapAccess, ModelDispatchRejection, ModelErrorContext, ModelErrorContextError, ModelErrorKind,
    ModelErrorPhase, ModelErrorTarget, ModelPaidRecovery, ModelPreparationError,
    ModelPreparationErrorKind, ModelRetryability, NonZeroU32, PreparedCallError,
    RedactedModelErrorSource, SafeDiagnostic, Serialize, Serializer, ValidationError, Visitor, fmt,
};

/// A provider-neutral typed model error with a redaction-safe message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelError {
    kind: ModelErrorKind,
    message: SafeDiagnostic,
    context: Option<Box<ModelErrorContext>>,
    context_capacity: Option<Box<ContextCapacityFailure>>,
    source: Option<Box<RedactedModelErrorSource>>,
    paid_recovery: Option<Box<ModelPaidRecovery>>,
    // Boxed so the output-limit wire maximum can live on this error without
    // pushing `Result<_, ModelError>` over clippy's large-err threshold.
    dispatch_rejection: Option<Box<ModelDispatchRejection>>,
    /// Prepared wire output maximum, when this is an output-limit failure.
    ///
    /// Absent on every other kind, and absent when the provider recorded no
    /// wire maximum. Not a new error kind: old records omit the field.
    wire_output_limit: Option<NonZeroU32>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum ModelErrorWireKind {
    InvalidConfiguration,
    InvalidRequest,
    UnsupportedCapability,
    ExpectedRequestMismatch,
    ScriptExhausted,
    InvalidEvent,
    IncompleteResponse,
    ContentFilteredIncomplete,
    Policy,
    OutcomeUnknown,
    Cancelled,
    Timeout,
    Authentication,
    ReauthenticationRequired,
    Authorization,
    RateLimited,
    QuotaExceeded,
    ContextLimit,
    ProtectedHistory,
    OutputLimit,
    Transport,
    Unavailable,
    ResourceLimit,
    Protocol,
    PaidRecoveryRequiredV1,
}

impl From<ModelErrorKind> for ModelErrorWireKind {
    fn from(kind: ModelErrorKind) -> Self {
        match kind {
            ModelErrorKind::InvalidConfiguration => Self::InvalidConfiguration,
            ModelErrorKind::InvalidRequest => Self::InvalidRequest,
            ModelErrorKind::UnsupportedCapability => Self::UnsupportedCapability,
            ModelErrorKind::ExpectedRequestMismatch => Self::ExpectedRequestMismatch,
            ModelErrorKind::ScriptExhausted => Self::ScriptExhausted,
            ModelErrorKind::InvalidEvent => Self::InvalidEvent,
            ModelErrorKind::IncompleteResponse => Self::IncompleteResponse,
            ModelErrorKind::ContentFilteredIncomplete => Self::ContentFilteredIncomplete,
            ModelErrorKind::Policy => Self::Policy,
            ModelErrorKind::OutcomeUnknown => Self::OutcomeUnknown,
            ModelErrorKind::Cancelled => Self::Cancelled,
            ModelErrorKind::Timeout => Self::Timeout,
            ModelErrorKind::Authentication => Self::Authentication,
            ModelErrorKind::ReauthenticationRequired => Self::ReauthenticationRequired,
            ModelErrorKind::Authorization => Self::Authorization,
            ModelErrorKind::RateLimited => Self::RateLimited,
            ModelErrorKind::QuotaExceeded => Self::QuotaExceeded,
            ModelErrorKind::ContextLimit => Self::ContextLimit,
            ModelErrorKind::ProtectedHistory => Self::ProtectedHistory,
            ModelErrorKind::OutputLimit => Self::OutputLimit,
            ModelErrorKind::Transport => Self::Transport,
            ModelErrorKind::Unavailable => Self::Unavailable,
            ModelErrorKind::ResourceLimit => Self::ResourceLimit,
            ModelErrorKind::Protocol => Self::Protocol,
        }
    }
}

impl ModelErrorWireKind {
    const fn public_kind(self) -> ModelErrorKind {
        match self {
            Self::InvalidConfiguration => ModelErrorKind::InvalidConfiguration,
            Self::InvalidRequest => ModelErrorKind::InvalidRequest,
            Self::UnsupportedCapability => ModelErrorKind::UnsupportedCapability,
            Self::ExpectedRequestMismatch => ModelErrorKind::ExpectedRequestMismatch,
            Self::ScriptExhausted => ModelErrorKind::ScriptExhausted,
            Self::InvalidEvent => ModelErrorKind::InvalidEvent,
            Self::IncompleteResponse => ModelErrorKind::IncompleteResponse,
            Self::ContentFilteredIncomplete => ModelErrorKind::ContentFilteredIncomplete,
            Self::Policy => ModelErrorKind::Policy,
            Self::OutcomeUnknown | Self::PaidRecoveryRequiredV1 => ModelErrorKind::OutcomeUnknown,
            Self::Cancelled => ModelErrorKind::Cancelled,
            Self::Timeout => ModelErrorKind::Timeout,
            Self::Authentication => ModelErrorKind::Authentication,
            Self::ReauthenticationRequired => ModelErrorKind::ReauthenticationRequired,
            Self::Authorization => ModelErrorKind::Authorization,
            Self::RateLimited => ModelErrorKind::RateLimited,
            Self::QuotaExceeded => ModelErrorKind::QuotaExceeded,
            Self::ContextLimit => ModelErrorKind::ContextLimit,
            Self::ProtectedHistory => ModelErrorKind::ProtectedHistory,
            Self::OutputLimit => ModelErrorKind::OutputLimit,
            Self::Transport => ModelErrorKind::Transport,
            Self::Unavailable => ModelErrorKind::Unavailable,
            Self::ResourceLimit => ModelErrorKind::ResourceLimit,
            Self::Protocol => ModelErrorKind::Protocol,
        }
    }

    const fn requires_paid_recovery(self) -> bool {
        matches!(self, Self::PaidRecoveryRequiredV1)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelErrorWire {
    kind: ModelErrorWireKind,
    message: SafeDiagnostic,
    #[serde(default)]
    context: Option<Box<ModelErrorContext>>,
    #[serde(default)]
    context_capacity: Option<Box<ContextCapacityFailure>>,
    #[serde(default)]
    dispatch_rejection: Option<ModelDispatchRejection>,
    #[serde(default)]
    source: Option<Box<RedactedModelErrorSource>>,
    #[serde(default)]
    paid_recovery: PaidRecoveryPresence,
    #[serde(default)]
    wire_output_limit: Option<NonZeroU32>,
}

struct ModelErrorWireVisitor;

impl<'de> Visitor<'de> for ModelErrorWireVisitor {
    type Value = ModelErrorWire;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical model error object")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        ModelErrorWire::deserialize(serde::de::value::MapAccessDeserializer::new(map))
    }
}

#[derive(Serialize)]
struct ModelErrorWireRef<'a> {
    kind: ModelErrorWireKind,
    message: &'a SafeDiagnostic,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a ModelErrorContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_capacity: Option<&'a ContextCapacityFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch_rejection: Option<ModelDispatchRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a RedactedModelErrorSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    paid_recovery: Option<&'a ModelPaidRecovery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wire_output_limit: Option<NonZeroU32>,
}

#[derive(Default)]
struct PaidRecoveryPresence(Option<Box<ModelPaidRecovery>>);

impl<'de> Deserialize<'de> for PaidRecoveryPresence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Box::<ModelPaidRecovery>::deserialize(deserializer).map(|recovery| Self(Some(recovery)))
    }
}

impl<'de> Deserialize<'de> for ModelError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = deserializer.deserialize_map(ModelErrorWireVisitor)?;
        let kind = wire.kind.public_kind();
        if wire.kind.requires_paid_recovery() != wire.paid_recovery.0.is_some() {
            return Err(serde::de::Error::custom(
                "paid recovery requires the versioned paid-recovery wire kind",
            ));
        }
        if let Some(context) = wire.context.as_deref() {
            validate_error_retry(kind, context.retryability()).map_err(serde::de::Error::custom)?;
        }
        if wire.context_capacity.is_some() && kind != ModelErrorKind::ContextLimit {
            return Err(serde::de::Error::custom(
                "context capacity evidence requires a context-limit error",
            ));
        }
        validate_paid_recovery_semantics(
            kind,
            wire.context.as_deref(),
            wire.paid_recovery.0.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        validate_dispatch_rejection_semantics(
            kind,
            wire.context.as_deref(),
            wire.dispatch_rejection,
            wire.paid_recovery.0.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        if wire.wire_output_limit.is_some() && kind != ModelErrorKind::OutputLimit {
            return Err(serde::de::Error::custom(
                "a wire output limit requires an output-limit error",
            ));
        }
        Ok(Self {
            kind,
            message: wire.message,
            context: wire.context,
            context_capacity: wire.context_capacity,
            dispatch_rejection: wire.dispatch_rejection.map(Box::new),
            source: wire.source,
            paid_recovery: wire.paid_recovery.0,
            wire_output_limit: wire.wire_output_limit,
        })
    }
}

impl Serialize for ModelError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_dispatch_rejection_semantics(
            self.kind,
            self.context.as_deref(),
            self.dispatch_rejection.as_deref().copied(),
            self.paid_recovery.as_deref(),
        )
        .map_err(serde::ser::Error::custom)?;
        ModelErrorWireRef {
            kind: if self.paid_recovery.is_some() {
                ModelErrorWireKind::PaidRecoveryRequiredV1
            } else {
                self.kind.into()
            },
            message: &self.message,
            context: self.context.as_deref(),
            context_capacity: self.context_capacity.as_deref(),
            dispatch_rejection: self.dispatch_rejection.as_deref().copied(),
            source: self.source.as_deref(),
            paid_recovery: self.paid_recovery.as_deref(),
            wire_output_limit: self.wire_output_limit,
        }
        .serialize(serializer)
    }
}

impl ModelError {
    /// Creates a context-free local or legacy error with a non-secret message.
    ///
    /// Errors crossing a resolved [`LanguageModel`](crate::LanguageModel) boundary must instead use
    /// [`Self::contextual`] or attach context exactly once before projection.
    #[must_use]
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message.into()),
            context: None,
            context_capacity: None,
            dispatch_rejection: None,
            source: None,
            paid_recovery: None,
            wire_output_limit: None,
        }
    }

    /// Creates an error with immutable resolved-operation attribution.
    ///
    /// # Errors
    ///
    /// Returns an error when retry disposition contradicts the stable category.
    pub fn contextual(
        kind: ModelErrorKind,
        message: impl Into<String>,
        context: ModelErrorContext,
    ) -> Result<Self, ModelErrorContextError> {
        validate_error_retry(kind, context.retryability())?;
        let mut error = Self::new(kind, message);
        error.context = Some(Box::new(context));
        Ok(error)
    }

    /// Creates an outcome-unknown, permanently non-retryable error for an exact
    /// validated target and provider-neutral paid recovery attachment.
    #[must_use]
    pub fn paid_recovery_required(
        message: impl Into<String>,
        target: &ModelErrorTarget,
        phase: ModelErrorPhase,
        paid_recovery: ModelPaidRecovery,
    ) -> Self {
        let context = ModelErrorContext {
            protocol: target.protocol.clone(),
            route: target.route.clone(),
            model: target.model.clone(),
            phase,
            retryability: ModelRetryability::Permanent,
            http_status: None,
            provider_code: None,
            provider_request_id: None,
        };
        Self {
            kind: ModelErrorKind::OutcomeUnknown,
            message: SafeDiagnostic::new(message.into()),
            context: Some(Box::new(context)),
            context_capacity: None,
            dispatch_rejection: None,
            source: None,
            paid_recovery: Some(Box::new(paid_recovery)),
            wire_output_limit: None,
        }
    }

    /// Attaches immutable operation context exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error if context is already present or retry disposition
    /// contradicts this error's stable category.
    pub fn attach_context(
        &mut self,
        context: ModelErrorContext,
    ) -> Result<(), ModelErrorContextError> {
        if self.context.is_some() {
            return Err(ModelErrorContextError::ContextAlreadySet);
        }
        validate_error_retry(self.kind, context.retryability())?;
        validate_dispatch_rejection_semantics(
            self.kind,
            Some(&context),
            self.dispatch_rejection.as_deref().copied(),
            self.paid_recovery.as_deref(),
        )?;
        self.context = Some(Box::new(context));
        Ok(())
    }

    /// Attaches bounded provider-neutral paid recovery evidence exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error when evidence is already attached or this error is not
    /// an attributed [`ModelErrorKind::OutcomeUnknown`] with
    /// [`ModelRetryability::Permanent`].
    pub fn attach_paid_recovery(
        &mut self,
        paid_recovery: ModelPaidRecovery,
    ) -> Result<(), ModelErrorContextError> {
        if self.paid_recovery.is_some() {
            return Err(ModelErrorContextError::PaidRecoveryAlreadySet);
        }
        validate_paid_recovery_semantics(self.kind, self.context.as_deref(), Some(&paid_recovery))?;
        validate_dispatch_rejection_semantics(
            self.kind,
            self.context.as_deref(),
            self.dispatch_rejection.as_deref().copied(),
            Some(&paid_recovery),
        )?;
        self.paid_recovery = Some(Box::new(paid_recovery));
        Ok(())
    }

    /// Attaches definite pre-dispatch rejection evidence.
    ///
    /// # Errors
    ///
    /// Rejects outcome-unknown, post-dispatch, terminal, or paid-recovery
    /// combinations because those facts cannot prove zero physical dispatch.
    pub fn with_dispatch_rejection(
        mut self,
        rejection: ModelDispatchRejection,
    ) -> Result<Self, ModelErrorContextError> {
        validate_dispatch_rejection_semantics(
            self.kind,
            self.context.as_deref(),
            Some(rejection),
            self.paid_recovery.as_deref(),
        )?;
        self.dispatch_rejection = Some(Box::new(rejection));
        Ok(self)
    }

    /// Fills missing attribution from a validated target without replacing any
    /// provider-supplied context.
    ///
    /// The synthesized retry disposition is always [`ModelRetryability::Unknown`]
    /// so this compatibility path can never authorize replay.
    pub fn ensure_context(&mut self, target: &ModelErrorTarget, phase: ModelErrorPhase) {
        if self.context.is_none() {
            let context = target.unknown_context(phase);
            if validate_dispatch_rejection_semantics(
                self.kind,
                Some(&context),
                self.dispatch_rejection.as_deref().copied(),
                self.paid_recovery.as_deref(),
            )
            .is_ok()
            {
                self.context = Some(Box::new(context));
            }
        }
    }

    /// Records the prepared wire output maximum on an output-limit failure.
    ///
    /// A value already recorded is kept. Any other kind ignores the call, so a
    /// transport failure cannot grow an output-limit field.
    #[must_use]
    pub fn with_wire_output_limit(mut self, tokens: NonZeroU32) -> Self {
        self.fill_missing_wire_output_limit(tokens);
        self
    }

    /// Records the prepared wire output maximum when this error does not have one.
    pub fn fill_missing_wire_output_limit(&mut self, tokens: NonZeroU32) {
        if self.kind == ModelErrorKind::OutputLimit && self.wire_output_limit.is_none() {
            self.wire_output_limit = Some(tokens);
        }
    }

    /// Returns the prepared wire output maximum, when this failure carries one.
    #[must_use]
    pub const fn wire_output_limit(&self) -> Option<NonZeroU32> {
        self.wire_output_limit
    }

    /// Attaches a bounded, sanitized lower-level source.
    #[must_use]
    pub fn with_source(mut self, source: impl AsRef<str>) -> Self {
        self.source = Some(Box::new(RedactedModelErrorSource::new(source)));
        self
    }

    /// Creates a cancellation error.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(ModelErrorKind::Cancelled, "model operation cancelled")
    }

    /// Creates a premature-EOF error.
    #[must_use]
    pub fn incomplete() -> Self {
        Self::new(
            ModelErrorKind::IncompleteResponse,
            "stream ended before a terminal aggregate",
        )
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ModelErrorKind {
        self.kind
    }

    /// Returns typed definite pre-dispatch rejection evidence, when present.
    #[must_use]
    pub fn dispatch_rejection(&self) -> Option<ModelDispatchRejection> {
        self.dispatch_rejection.as_deref().copied()
    }

    pub(in super::super) fn bind_dispatch_rejection(
        &mut self,
        prepared_call: Digest,
        attempt_id: AttemptId,
    ) {
        if let Some(rejection) = &mut self.dispatch_rejection {
            rejection.bind_exact(prepared_call, attempt_id);
        }
    }

    /// Returns the exact-projection capacity classification retained at preparation.
    #[must_use]
    pub fn context_capacity(&self) -> Option<ContextCapacityFailure> {
        self.context_capacity.as_deref().copied()
    }

    /// Reports whether the provider refused the request because its input
    /// exceeded the context window, as opposed to local admission refusing it
    /// before dispatch. Such a refusal is definite: nothing was generated.
    #[must_use]
    pub fn is_provider_context_overflow(&self) -> bool {
        self.kind == ModelErrorKind::ContextLimit && self.context_capacity.is_none()
    }

    /// Returns the redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        self.message.as_str()
    }

    /// Appends one kernel-owned attempt-accounting suffix, preserving typed context.
    #[must_use]
    pub fn with_attempt_summary(mut self, summary: impl fmt::Display) -> Self {
        let suffix = SafeDiagnostic::new(summary.to_string());
        let maximum = crate::MAX_SAFE_DIAGNOSTIC_BYTES.saturating_sub(suffix.as_str().len());
        let prefix = self.message.as_str();
        let mut end = prefix.len().min(maximum);
        while !prefix.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        self.message = SafeDiagnostic::new(format!("{}{suffix}", &prefix[..end]));
        self
    }

    /// Returns the retained bounded, sanitized diagnostic value.
    #[must_use]
    pub const fn diagnostic(&self) -> &SafeDiagnostic {
        &self.message
    }

    /// Returns safe resolved-operation context, when available.
    #[must_use]
    pub fn context(&self) -> Option<&ModelErrorContext> {
        self.context.as_deref()
    }

    /// Returns paid-recovery evidence when explicit recovery is required.
    #[must_use]
    pub fn paid_recovery(&self) -> Option<&ModelPaidRecovery> {
        self.paid_recovery.as_deref()
    }

    /// Returns the retained bounded lower-level source, when available.
    #[must_use]
    pub fn source_diagnostic(&self) -> Option<&RedactedModelErrorSource> {
        self.source.as_deref()
    }
}

fn validate_paid_recovery_semantics(
    kind: ModelErrorKind,
    context: Option<&ModelErrorContext>,
    paid_recovery: Option<&ModelPaidRecovery>,
) -> Result<(), ModelErrorContextError> {
    if paid_recovery.is_none() {
        return Ok(());
    }
    if kind != ModelErrorKind::OutcomeUnknown {
        return Err(ModelErrorContextError::PaidRecoveryRequiresOutcomeUnknown);
    }
    let context = context.ok_or(ModelErrorContextError::PaidRecoveryRequiresContext)?;
    if context.retryability() != ModelRetryability::Permanent {
        return Err(ModelErrorContextError::PaidRecoveryRequiresPermanentRetry);
    }
    Ok(())
}

fn validate_dispatch_rejection_semantics(
    kind: ModelErrorKind,
    context: Option<&ModelErrorContext>,
    dispatch_rejection: Option<ModelDispatchRejection>,
    paid_recovery: Option<&ModelPaidRecovery>,
) -> Result<(), ModelErrorContextError> {
    if dispatch_rejection.is_none() {
        return Ok(());
    }
    let post_dispatch_phase = context.is_some_and(|context| {
        matches!(
            context.phase(),
            ModelErrorPhase::RequestWrite
                | ModelErrorPhase::FirstEvent
                | ModelErrorPhase::Stream
                | ModelErrorPhase::StreamIdle
                | ModelErrorPhase::Terminal
                | ModelErrorPhase::Attempt
                | ModelErrorPhase::Operation
                | ModelErrorPhase::Cleanup
        )
    });
    if kind == ModelErrorKind::OutcomeUnknown || paid_recovery.is_some() || post_dispatch_phase {
        Err(ModelErrorContextError::ContradictoryDispatchRejection)
    } else {
        Ok(())
    }
}

fn validate_error_retry(
    kind: ModelErrorKind,
    retryability: ModelRetryability,
) -> Result<(), ModelErrorContextError> {
    let permanently_nonretryable = matches!(
        kind,
        ModelErrorKind::InvalidConfiguration
            | ModelErrorKind::InvalidRequest
            | ModelErrorKind::UnsupportedCapability
            | ModelErrorKind::ExpectedRequestMismatch
            | ModelErrorKind::ScriptExhausted
            | ModelErrorKind::InvalidEvent
            | ModelErrorKind::OutcomeUnknown
            | ModelErrorKind::Cancelled
            | ModelErrorKind::Authentication
            | ModelErrorKind::ReauthenticationRequired
            | ModelErrorKind::Authorization
            | ModelErrorKind::QuotaExceeded
            | ModelErrorKind::ContextLimit
            | ModelErrorKind::ProtectedHistory
            | ModelErrorKind::OutputLimit
            | ModelErrorKind::Policy
            | ModelErrorKind::ResourceLimit
    );
    if permanently_nonretryable
        && matches!(
            retryability,
            ModelRetryability::Eligible { .. } | ModelRetryability::Exhausted
        )
    {
        Err(ModelErrorContextError::ContradictoryRetryDisposition)
    } else {
        Ok(())
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl From<ValidationError> for ModelError {
    fn from(error: ValidationError) -> Self {
        let kind = match error {
            ValidationError::RequiredCapability { .. }
            | ValidationError::StrictPreference { .. } => ModelErrorKind::UnsupportedCapability,
            ValidationError::ForeignPlan
            | ValidationError::SubstitutedRequest
            | ValidationError::Serialization { .. }
            | ValidationError::Digest(_) => ModelErrorKind::InvalidRequest,
        };
        Self::new(kind, error.to_string())
    }
}

impl From<ModelPreparationError> for ModelError {
    fn from(error: ModelPreparationError) -> Self {
        let context_capacity = match &error {
            ModelPreparationError::ContextAdmissionDenied { status, .. } => {
                ContextCapacityFailure::from_status(status)
            }
            _ => None,
        };
        let kind = match &error {
            ModelPreparationError::Validation(validation) => match validation {
                ValidationError::RequiredCapability { .. }
                | ValidationError::StrictPreference { .. } => ModelErrorKind::UnsupportedCapability,
                ValidationError::ForeignPlan
                | ValidationError::SubstitutedRequest
                | ValidationError::Serialization { .. }
                | ValidationError::Digest(_) => ModelErrorKind::InvalidRequest,
            },
            ModelPreparationError::RuntimeAuthority(_) => ModelErrorKind::InvalidConfiguration,
            ModelPreparationError::PreparedCall(PreparedCallError::RecordTooLarge) => {
                ModelErrorKind::ContextLimit
            }
            ModelPreparationError::PreparedCall(_) => ModelErrorKind::InvalidRequest,
            ModelPreparationError::ContextAdmission(error) => match error {
                ContextAdmissionError::WouldOverflow(_)
                | ContextAdmissionError::ProtectedOverflow(_) => ModelErrorKind::ContextLimit,
                ContextAdmissionError::Unavailable(_)
                | ContextAdmissionError::Contract(_)
                | ContextAdmissionError::Digest(_) => ModelErrorKind::InvalidRequest,
            },
            ModelPreparationError::ContextAdmissionDenied { error, .. } => match error {
                ContextAdmissionError::WouldOverflow(_)
                | ContextAdmissionError::ProtectedOverflow(_) => ModelErrorKind::ContextLimit,
                ContextAdmissionError::Unavailable(_)
                | ContextAdmissionError::Contract(_)
                | ContextAdmissionError::Digest(_) => ModelErrorKind::InvalidRequest,
            },
            ModelPreparationError::Rejected { kind, .. } => match kind {
                ModelPreparationErrorKind::InvalidRequest
                | ModelPreparationErrorKind::ArtifactReuse
                | ModelPreparationErrorKind::Projection => ModelErrorKind::InvalidRequest,
                ModelPreparationErrorKind::UnsupportedCapability => {
                    ModelErrorKind::UnsupportedCapability
                }
                ModelPreparationErrorKind::ProtectedHistory => ModelErrorKind::ProtectedHistory,
                ModelPreparationErrorKind::ResourceLimit => ModelErrorKind::ResourceLimit,
                ModelPreparationErrorKind::InvalidConfiguration => {
                    ModelErrorKind::InvalidConfiguration
                }
            },
        };
        let mut projected = Self::new(kind, error.to_string());
        projected.context_capacity = context_capacity.map(Box::new);
        projected
    }
}
