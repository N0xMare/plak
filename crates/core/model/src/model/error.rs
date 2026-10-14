//! Typed model failures, retry dispositions, and paid recovery attachments.

use std::fmt;
use std::num::NonZeroU32;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::{ModelPreparationError, ModelPreparationErrorKind, PreparedCallError};
use crate::{
    AttemptId, ContextAdmissionError, ContextOverflow, ContextStatus, ContextStatusKind, Digest,
    ModelId, ProtocolId, ResolvedModelDescriptor, RouteId, SafeDiagnostic, ValidationError,
};

/// The stable category of a model failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelErrorKind {
    /// Model or route configuration is invalid.
    InvalidConfiguration,
    /// Request construction or preparation failed.
    InvalidRequest,
    /// A required capability was unavailable.
    UnsupportedCapability,
    /// A scripted expectation did not match the prepared request.
    ExpectedRequestMismatch,
    /// No scripted operation remained.
    ScriptExhausted,
    /// A stream event violated structural sequencing rules.
    InvalidEvent,
    /// EOF arrived before a terminal aggregate.
    IncompleteResponse,
    /// A sampling response was incomplete because its content was filtered.
    ContentFilteredIncomplete,
    /// A content policy refusal is terminal for this turn.
    Policy,
    /// A durable physical-request transition may have completed and requires
    /// explicit recovery before the operation can be recreated.
    OutcomeUnknown,
    /// The operation was cancelled.
    Cancelled,
    /// The operation deadline elapsed.
    Timeout,
    /// Provider or route authentication failed.
    Authentication,
    /// The selected subscription route requires interactive reauthentication.
    ReauthenticationRequired,
    /// The authenticated principal is not authorized for the operation.
    Authorization,
    /// The remote service reported rate limiting.
    RateLimited,
    /// The selected account or project has no usable quota.
    QuotaExceeded,
    /// The request exceeded the model or route context limit.
    ContextLimit,
    /// Protected native history cannot be replayed under the selected binding.
    ProtectedHistory,
    /// The response exceeded a configured or provider output limit.
    OutputLimit,
    /// The network transport failed before a protocol result was available.
    Transport,
    /// The selected route or model is temporarily unavailable.
    Unavailable,
    /// A local or remote bounded resource limit was exceeded.
    ResourceLimit,
    /// The protocol operation failed.
    Protocol,
}

/// Provider-neutral phase in which a model operation failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelErrorPhase {
    /// Canonical validation or provider request preparation.
    Preparation,
    /// Credential resolution, refresh, or authorization.
    Authentication,
    /// Operation-local service readiness or admission.
    Readiness,
    /// DNS, TCP, or TLS establishment.
    Connect,
    /// Request headers or body transmission.
    RequestWrite,
    /// Waiting for the first response event.
    FirstEvent,
    /// Incremental response transport or decoding.
    Stream,
    /// An established stream exceeded its idle deadline.
    StreamIdle,
    /// Terminal response validation or projection.
    Terminal,
    /// One physical request exceeded its attempt deadline.
    Attempt,
    /// The complete logical operation exceeded its total deadline.
    Operation,
    /// Joined cancellation or resource cleanup.
    Cleanup,
}

/// Redaction-safe class of a provider-family dispatch rejection which proved
/// that no physical request crossed the transport boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelDispatchRejectionClass {
    /// Route, model, account, request, or operation authority differed.
    Binding,
    /// Credential acquisition or freshness validation failed.
    Authentication,
    /// Current route or account authority denied dispatch.
    Authorization,
    /// A bounded local admission authority denied dispatch.
    Admission,
    /// Required local authority was unavailable.
    Unavailable,
}

/// Exact provider-family fence which definitely rejected dispatch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelDispatchRejectionPhase {
    /// Dialect preflight rejected before credentialized request construction.
    Preflight,
    /// Final revalidation rejected the exact prepared physical request.
    Revalidation,
}

/// Typed, content-free evidence for a definite pre-dispatch rejection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ModelDispatchRejection {
    class: ModelDispatchRejectionClass,
    phase: ModelDispatchRejectionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_call: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_id: Option<AttemptId>,
}

impl ModelDispatchRejection {
    /// Creates closed rejection evidence.
    #[must_use]
    pub const fn new(
        class: ModelDispatchRejectionClass,
        phase: ModelDispatchRejectionPhase,
    ) -> Self {
        Self {
            class,
            phase,
            prepared_call: None,
            attempt_id: None,
        }
    }

    /// Creates proof bound to one exact prepared call and attempt.
    ///
    /// Provider-family hosts normally emit an unbound rejection and let
    /// [`PreparedModelCall`](super::PreparedModelCall) seal these axes at its one-shot boundary. This
    /// constructor supports durable recovery validation and exact test seams.
    #[must_use]
    pub const fn for_prepared_call(
        class: ModelDispatchRejectionClass,
        phase: ModelDispatchRejectionPhase,
        prepared_call: Digest,
        attempt_id: AttemptId,
    ) -> Self {
        Self {
            class,
            phase,
            prepared_call: Some(prepared_call),
            attempt_id: Some(attempt_id),
        }
    }

    /// Returns the redaction-safe denial class.
    #[must_use]
    pub const fn class(self) -> ModelDispatchRejectionClass {
        self.class
    }

    /// Returns the exact rejecting fence.
    #[must_use]
    pub const fn phase(self) -> ModelDispatchRejectionPhase {
        self.phase
    }

    /// Returns whether this proof is bound to the exact runtime operation.
    #[must_use]
    pub fn matches(self, prepared_call: Digest, attempt_id: AttemptId) -> bool {
        self.prepared_call == Some(prepared_call) && self.attempt_id == Some(attempt_id)
    }

    fn bind_exact(&mut self, prepared_call: Digest, attempt_id: AttemptId) {
        if self.prepared_call.is_none() && self.attempt_id.is_none() {
            self.prepared_call = Some(prepared_call);
            self.attempt_id = Some(attempt_id);
        }
    }
}

/// Retry and duplicate-effect disposition recorded on a terminal model error.
///
/// This value is descriptive. It never authorizes an agent or caller to add a
/// second retry loop around a protocol service which owns its retries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelRetryability {
    /// No trustworthy retry classification is available; replay must fail closed.
    Unknown,
    /// The failure is permanent or otherwise not retryable.
    Permanent,
    /// The protocol owner may retry before acceptance, subject to its own policy.
    Eligible {
        /// Bounded provider advice; the protocol owner still applies its local cap.
        /// Eligibility requires proof of non-acceptance or a documented
        /// idempotent/resumable mechanism.
        retry_after: Option<ModelRetryDelay>,
    },
    /// A safe non-acceptance or documented-resume retry path exhausted its budget.
    /// This variant never carries duplicate inference or hosted-effect risk.
    Exhausted,
    /// Recreating the operation may duplicate inference or billing work.
    Unsafe {
        /// The strongest known duplicate-effect risk.
        risk: ModelRetryRisk,
    },
}

/// Duplicate-effect risk remaining after a model operation failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelRetryRisk {
    /// Recreating the request may duplicate inference or billed work only.
    DuplicateInferenceOrBilling,
    /// Recreating the request may duplicate a provider-hosted external effect.
    ProviderHostedEffect,
}

/// Bounded provider retry-delay advice in milliseconds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct ModelRetryDelay(u64);

impl ModelRetryDelay {
    /// Maximum provider retry delay retained in a public error: 24 hours.
    pub const MAX_MILLIS: u64 = 86_400_000;

    /// Validates bounded retry-delay advice.
    ///
    /// # Errors
    ///
    /// Returns an error when the value exceeds the public retention ceiling.
    pub const fn from_millis(millis: u64) -> Result<Self, ModelErrorContextError> {
        if millis <= Self::MAX_MILLIS {
            Ok(Self(millis))
        } else {
            Err(ModelErrorContextError::RetryDelayTooLarge)
        }
    }

    /// Returns the retained delay in milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for ModelRetryDelay {
    type Error = ModelErrorContextError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::from_millis(value)
    }
}

impl From<ModelRetryDelay> for u64 {
    fn from(value: ModelRetryDelay) -> Self {
        value.0
    }
}

/// A validated HTTP response status retained in public model-error context.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelHttpStatus(u16);

impl ModelHttpStatus {
    /// Validates an HTTP response status.
    ///
    /// # Errors
    ///
    /// Returns an error for values outside the registered three-digit status range.
    pub const fn new(status: u16) -> Result<Self, ModelErrorContextError> {
        if status >= 100 && status <= 599 {
            Ok(Self(status))
        } else {
            Err(ModelErrorContextError::InvalidHttpStatus(status))
        }
    }

    /// Returns the numeric status code.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ModelHttpStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let status = u16::deserialize(deserializer)?;
        Self::new(status).map_err(serde::de::Error::custom)
    }
}

/// Invalid provider-neutral model-error metadata.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ModelErrorContextError {
    /// An HTTP status must be a registered three-digit response status.
    #[error("invalid HTTP response status: {0}")]
    InvalidHttpStatus(u16),
    /// Provider retry advice exceeded the public retention ceiling.
    #[error("provider retry delay exceeds the public retention ceiling")]
    RetryDelayTooLarge,
    /// A provider error code did not satisfy the bounded opaque grammar.
    #[error("invalid provider error code")]
    InvalidProviderCode,
    /// A provider request ID did not satisfy the bounded opaque grammar.
    #[error("invalid provider request identifier")]
    InvalidProviderRequestId,
    /// A resolved target identity was credential-shaped or otherwise unsafe to retain.
    #[error("model error target identity is not safe for public retention")]
    UnsafeTargetIdentity,
    /// A context was attached after immutable attribution had already been frozen.
    #[error("model error context is already set")]
    ContextAlreadySet,
    /// Retry disposition contradicted the stable error category.
    #[error("model error retry disposition contradicts its error category")]
    ContradictoryRetryDisposition,
    /// Canonical paid-recovery evidence was empty.
    #[error("paid recovery evidence is empty")]
    EmptyPaidRecovery,
    /// Serialized paid-recovery evidence was not canonical lowercase hexadecimal.
    #[error("paid recovery evidence encoding is malformed or noncanonical")]
    MalformedPaidRecoveryEncoding,
    /// Canonical paid-recovery evidence exceeded its public retention ceiling.
    #[error("paid recovery evidence exceeds the public retention ceiling")]
    PaidRecoveryTooLarge,
    /// Paid terminal facts contradicted one another.
    #[error("paid recovery terminal facts are inconsistent")]
    InconsistentPaidTerminalFacts,
    /// Paid-recovery evidence was attached more than once.
    #[error("paid recovery evidence is already set")]
    PaidRecoveryAlreadySet,
    /// Paid-recovery evidence was attached to a non-ambiguous error category.
    #[error("paid recovery evidence requires an outcome-unknown error")]
    PaidRecoveryRequiresOutcomeUnknown,
    /// Paid-recovery evidence lacked immutable resolved-operation context.
    #[error("paid recovery evidence requires model error context")]
    PaidRecoveryRequiresContext,
    /// Paid-recovery evidence carried a retry disposition other than permanent.
    #[error("paid recovery evidence requires permanent retry disposition")]
    PaidRecoveryRequiresPermanentRetry,
    /// Definite pre-dispatch evidence contradicted outcome or phase facts.
    #[error("dispatch rejection evidence contradicts model error outcome facts")]
    ContradictoryDispatchRejection,
}

/// Maximum bytes retained for a validated provider error code.
pub const MAX_MODEL_PROVIDER_CODE_BYTES: usize = 128;

/// Maximum bytes retained for a validated provider request identifier.
pub const MAX_MODEL_PROVIDER_REQUEST_ID_BYTES: usize = 256;

/// Maximum bytes retained for a validated provider display name.
pub const MAX_SERVED_BY_BYTES: usize = 64;

/// A bounded, validated upstream provider display name reported on a model response.
///
/// This is the human-facing routing label reported by the gateway (for example
/// `Google AI Studio`), not an endpoint slug or URL path.
#[derive(Clone, Copy, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServedBy {
    len: u8,
    bytes: [u8; MAX_SERVED_BY_BYTES],
}

impl ServedBy {
    /// Validates a printable ASCII provider display name.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError`](crate::IdentityError) for empty, oversized, or non-printable values.
    pub fn new(value: impl AsRef<str>) -> Result<Self, crate::IdentityError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(crate::IdentityError::Empty);
        }
        if value.len() > MAX_SERVED_BY_BYTES {
            return Err(crate::IdentityError::TooLong {
                maximum: MAX_SERVED_BY_BYTES,
            });
        }
        let mut bytes = [0_u8; MAX_SERVED_BY_BYTES];
        for (index, byte) in value.bytes().enumerate() {
            if !(0x20..=0x7e).contains(&byte) {
                return Err(crate::IdentityError::InvalidCharacter { index });
            }
            bytes[index] = byte;
        }
        Ok(Self {
            len: u8::try_from(value.len()).map_err(|_| crate::IdentityError::TooLong {
                maximum: MAX_SERVED_BY_BYTES,
            })?,
            bytes,
        })
    }

    /// Returns the validated provider display name.
    ///
    /// `new` accepts only printable ASCII, which is always valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let slice = &self.bytes[..usize::from(self.len)];
        std::str::from_utf8(slice).unwrap_or("")
    }
}

impl TryFrom<String> for ServedBy {
    type Error = crate::IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ServedBy> for String {
    fn from(value: ServedBy) -> Self {
        value.as_str().to_owned()
    }
}

impl fmt::Debug for ServedBy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServedBy")
            .field("value", &self.as_str())
            .finish()
    }
}

/// Maximum canonical authority-recovery bytes retained on a model error.
///
/// The ceiling matches the durable paid-recovery journal ceiling. The bytes
/// are serialized as canonical lowercase hexadecimal, so their wire text is
/// exactly twice this size.
pub const MAX_MODEL_PAID_RECOVERY_BYTES: usize = 64 * 1024;

macro_rules! provider_metadata {
    ($name:ident, $maximum:expr, $error:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates bounded, non-secret opaque provider metadata.
            ///
            /// # Errors
            ///
            /// Returns an error for empty, oversized, nonportable, or
            /// credential-shaped values.
            pub fn new(value: impl Into<String>) -> Result<Self, ModelErrorContextError> {
                let value = value.into();
                let lowercase = value.to_ascii_lowercase();
                let valid_grammar = !value.is_empty()
                    && value.len() <= $maximum
                    && !lowercase.starts_with("bearer")
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
                    });
                if valid_grammar && SafeDiagnostic::new(&value).as_str() == value {
                    Ok(Self(value))
                } else {
                    Err($error)
                }
            }

            /// Returns the validated metadata text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ModelErrorContextError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("bytes", &self.0.len())
                    .field("value", &"<redacted>")
                    .finish()
            }
        }
    };
}

provider_metadata!(
    ModelProviderCode,
    MAX_MODEL_PROVIDER_CODE_BYTES,
    ModelErrorContextError::InvalidProviderCode,
    "A bounded, validated provider error code."
);
provider_metadata!(
    ModelProviderRequestId,
    MAX_MODEL_PROVIDER_REQUEST_ID_BYTES,
    ModelErrorContextError::InvalidProviderRequestId,
    "A bounded, validated provider request identifier."
);

/// Conservative provider acceptance retained for a paid physical operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelPaidProviderAcceptance {
    /// Every completed physical request was definitely rejected before acceptance.
    NotAccepted,
    /// At least one transmitted request may have been accepted.
    PossiblyAccepted,
    /// Response evidence proved provider acceptance.
    Accepted,
}

/// Sealed provider-neutral terminal facts for a paid physical operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "ModelPaidTerminalFactsWire")]
pub struct ModelPaidTerminalFacts {
    physical_attempts: u32,
    provider_acceptance: ModelPaidProviderAcceptance,
    visible_output: bool,
    hosted_effect_capable: bool,
    hosted_effects_possible: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPaidTerminalFactsWire {
    physical_attempts: u32,
    provider_acceptance: ModelPaidProviderAcceptance,
    visible_output: bool,
    hosted_effect_capable: bool,
    hosted_effects_possible: bool,
}

impl ModelPaidTerminalFacts {
    /// Validates and seals common terminal facts from a provider-owned attempt.
    ///
    /// # Errors
    ///
    /// Rejects facts that claim acceptance, output, or possible effects without
    /// a physical attempt; output without proved acceptance; or possible hosted
    /// effects without both capability and possible provider acceptance, or a
    /// hosted-effect-capable accepted request that understates possible effects.
    pub const fn new(
        physical_attempts: u32,
        provider_acceptance: ModelPaidProviderAcceptance,
        visible_output: bool,
        hosted_effect_capable: bool,
        hosted_effects_possible: bool,
    ) -> Result<Self, ModelErrorContextError> {
        let no_attempt_contradiction = physical_attempts == 0
            && (!matches!(
                provider_acceptance,
                ModelPaidProviderAcceptance::NotAccepted
            ) || visible_output
                || hosted_effects_possible);
        let output_contradiction =
            visible_output && !matches!(provider_acceptance, ModelPaidProviderAcceptance::Accepted);
        let effect_contradiction = hosted_effects_possible
            && (!hosted_effect_capable
                || matches!(
                    provider_acceptance,
                    ModelPaidProviderAcceptance::NotAccepted
                ));
        let understated_effect_risk = hosted_effect_capable
            && !matches!(
                provider_acceptance,
                ModelPaidProviderAcceptance::NotAccepted
            )
            && !hosted_effects_possible;
        if no_attempt_contradiction
            || output_contradiction
            || effect_contradiction
            || understated_effect_risk
        {
            Err(ModelErrorContextError::InconsistentPaidTerminalFacts)
        } else {
            Ok(Self {
                physical_attempts,
                provider_acceptance,
                visible_output,
                hosted_effect_capable,
                hosted_effects_possible,
            })
        }
    }

    /// Returns the number of physical provider requests owned by the operation.
    #[must_use]
    pub const fn physical_attempts(self) -> u32 {
        self.physical_attempts
    }

    /// Returns the conservative provider-acceptance state.
    #[must_use]
    pub const fn provider_acceptance(self) -> ModelPaidProviderAcceptance {
        self.provider_acceptance
    }

    /// Reports whether provider output became visible to the caller.
    #[must_use]
    pub const fn visible_output(self) -> bool {
        self.visible_output
    }

    /// Reports whether the immutable request could initiate hosted effects.
    #[must_use]
    pub const fn hosted_effect_capable(self) -> bool {
        self.hosted_effect_capable
    }

    /// Reports whether an opaque provider-hosted effect may have started.
    #[must_use]
    pub const fn hosted_effects_possible(self) -> bool {
        self.hosted_effects_possible
    }
}

impl TryFrom<ModelPaidTerminalFactsWire> for ModelPaidTerminalFacts {
    type Error = ModelErrorContextError;

    fn try_from(wire: ModelPaidTerminalFactsWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.physical_attempts,
            wire.provider_acceptance,
            wire.visible_output,
            wire.hosted_effect_capable,
            wire.hosted_effects_possible,
        )
    }
}

/// Bounded canonical bytes for credential-free paid recovery evidence.
///
/// This wrapper does not interpret authority-owned token formats. Its
/// constructor is for bytes produced by an authority recovery-token contract:
/// canonical and non-secret before they enter this crate. The evidence may be
/// an exact current transition token or a stable lineage locator whose durable
/// authority provides total read-only restart-safe resolution. Model code must
/// not treat either form as terminal proof or consuming mutation authority.
/// Serialization uses strict lowercase hexadecimal so arbitrary opaque bytes
/// cannot escape into JSON syntax, logs, `Debug`, or `Display`.
/// Deserialization uses a bounded borrowed-string visitor and allocates the
/// decoded bytes only after validating the wire length. A generic format may
/// itself allocate an owned string before invoking any Serde visitor; that
/// parser-owned allocation cannot be bounded by this value type.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ModelPaidRecoveryBytes(Vec<u8>);

impl ModelPaidRecoveryBytes {
    /// Validates canonical authority recovery bytes for bounded public retention.
    ///
    /// # Errors
    ///
    /// Rejects empty input or input larger than
    /// [`MAX_MODEL_PAID_RECOVERY_BYTES`].
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, ModelErrorContextError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            Err(ModelErrorContextError::EmptyPaidRecovery)
        } else if bytes.len() > MAX_MODEL_PAID_RECOVERY_BYTES {
            Err(ModelErrorContextError::PaidRecoveryTooLarge)
        } else {
            Ok(Self(bytes))
        }
    }

    /// Returns the canonical authority-owned recovery bytes unchanged.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn encode_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(self.0.len().saturating_mul(2));
        for byte in &self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    fn decode_hex(encoded: &str) -> Result<Self, ModelErrorContextError> {
        if encoded.is_empty() {
            return Err(ModelErrorContextError::EmptyPaidRecovery);
        }
        if encoded.len() > MAX_MODEL_PAID_RECOVERY_BYTES.saturating_mul(2) {
            return Err(ModelErrorContextError::PaidRecoveryTooLarge);
        }
        if !encoded.len().is_multiple_of(2) {
            return Err(ModelErrorContextError::MalformedPaidRecoveryEncoding);
        }
        let mut bytes = Vec::with_capacity(encoded.len() / 2);
        for pair in encoded.as_bytes().chunks_exact(2) {
            let high = decode_lower_hex(pair[0])
                .ok_or(ModelErrorContextError::MalformedPaidRecoveryEncoding)?;
            let low = decode_lower_hex(pair[1])
                .ok_or(ModelErrorContextError::MalformedPaidRecoveryEncoding)?;
            bytes.push((high << 4) | low);
        }
        Self::new(bytes)
    }
}

const fn decode_lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl Serialize for ModelPaidRecoveryBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.encode_hex())
    }
}

impl<'de> Deserialize<'de> for ModelPaidRecoveryBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ModelPaidRecoveryBytesVisitor)
    }
}

struct ModelPaidRecoveryBytesVisitor;

impl<'de> Visitor<'de> for ModelPaidRecoveryBytesVisitor {
    type Value = ModelPaidRecoveryBytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "1..={} canonical lowercase hexadecimal characters",
            MAX_MODEL_PAID_RECOVERY_BYTES.saturating_mul(2)
        )
    }

    fn visit_borrowed_str<E>(self, encoded: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        ModelPaidRecoveryBytes::decode_hex(encoded).map_err(E::custom)
    }

    fn visit_str<E>(self, encoded: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        ModelPaidRecoveryBytes::decode_hex(encoded).map_err(E::custom)
    }

    fn visit_string<E>(self, encoded: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        // Some generic formats allocate an owned string before invoking their
        // visitor. We still reject it before allocating the decoded byte vector;
        // borrowed formats enforce the bound before any attachment-owned allocation.
        ModelPaidRecoveryBytes::decode_hex(&encoded).map_err(E::custom)
    }
}

impl fmt::Debug for ModelPaidRecoveryBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelPaidRecoveryBytes")
            .field("bytes", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for ModelPaidRecoveryBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<{} paid recovery bytes redacted>", self.0.len())
    }
}

/// Explicit disposition carried only by bounded paid-recovery attachments.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelPaidRecoveryRequirement {
    /// The paid operation must be reconciled from its exact token before replay.
    PaidRecoveryRequired,
}

/// Credential-free bounded recovery evidence attached to a terminal model error.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelPaidRecovery {
    requirement: ModelPaidRecoveryRequirement,
    canonical_recovery_bytes: ModelPaidRecoveryBytes,
    terminal_facts: ModelPaidTerminalFacts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPaidRecoveryWire {
    requirement: ModelPaidRecoveryRequirement,
    canonical_recovery_bytes: ModelPaidRecoveryBytes,
    terminal_facts: ModelPaidTerminalFacts,
}

impl<'de> Deserialize<'de> for ModelPaidRecovery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelPaidRecoveryWire::deserialize(deserializer)?;
        if wire.requirement != ModelPaidRecoveryRequirement::PaidRecoveryRequired {
            return Err(serde::de::Error::custom(
                "invalid paid recovery requirement",
            ));
        }
        Ok(Self {
            requirement: wire.requirement,
            canonical_recovery_bytes: wire.canonical_recovery_bytes,
            terminal_facts: wire.terminal_facts,
        })
    }
}

impl ModelPaidRecovery {
    /// Creates an explicit paid-recovery-required attachment.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized canonical recovery bytes.
    pub fn new(
        canonical_recovery_bytes: impl Into<Vec<u8>>,
        terminal_facts: ModelPaidTerminalFacts,
    ) -> Result<Self, ModelErrorContextError> {
        Ok(Self {
            requirement: ModelPaidRecoveryRequirement::PaidRecoveryRequired,
            canonical_recovery_bytes: ModelPaidRecoveryBytes::new(canonical_recovery_bytes)?,
            terminal_facts,
        })
    }

    /// Returns the explicit recovery-required marker.
    #[must_use]
    pub const fn requirement(&self) -> ModelPaidRecoveryRequirement {
        self.requirement
    }

    /// Returns the canonical authority-owned recovery bytes unchanged.
    #[must_use]
    pub fn canonical_recovery_bytes(&self) -> &[u8] {
        self.canonical_recovery_bytes.as_bytes()
    }

    /// Returns the sealed provider-neutral terminal facts.
    #[must_use]
    pub const fn terminal_facts(&self) -> ModelPaidTerminalFacts {
        self.terminal_facts
    }
}

impl fmt::Debug for ModelPaidRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelPaidRecovery")
            .field("requirement", &self.requirement)
            .field("canonical_recovery_bytes", &self.canonical_recovery_bytes)
            .field("terminal_facts", &self.terminal_facts)
            .finish()
    }
}

impl fmt::Display for ModelPaidRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "paid recovery required ({})",
            self.canonical_recovery_bytes
        )
    }
}

/// Validated public attribution for one resolved model target.
///
/// A runtime can validate this once when it admits a [`LanguageModel`](super::LanguageModel) and use
/// it to conservatively contextualize legacy errors without revalidating or
/// replacing provider-supplied attribution.
#[derive(Clone, Eq, PartialEq)]
pub struct ModelErrorTarget {
    protocol: ProtocolId,
    route: RouteId,
    model: ModelId,
}

impl ModelErrorTarget {
    /// Validates public attribution for an exact resolved target.
    ///
    /// # Errors
    ///
    /// Returns an error when any identity is credential-shaped or would change
    /// under the shared public redaction policy.
    pub fn new(
        protocol: ProtocolId,
        route: RouteId,
        model: ModelId,
    ) -> Result<Self, ModelErrorContextError> {
        if !safe_target_identity(protocol.as_str())
            || !safe_target_identity(route.as_str())
            || !safe_target_identity(model.as_str())
        {
            return Err(ModelErrorContextError::UnsafeTargetIdentity);
        }
        Ok(Self {
            protocol,
            route,
            model,
        })
    }

    /// Validates public attribution borrowed from a resolved descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor contains an unsafe public identity.
    pub fn from_descriptor(
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<Self, ModelErrorContextError> {
        Self::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
        )
    }

    /// Returns the protocol-dialect identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the requested model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Creates context with a fail-closed retry disposition.
    #[must_use]
    pub fn unknown_context(&self, phase: ModelErrorPhase) -> ModelErrorContext {
        ModelErrorContext {
            protocol: self.protocol.clone(),
            route: self.route.clone(),
            model: self.model.clone(),
            phase,
            retryability: ModelRetryability::Unknown,
            http_status: None,
            provider_code: None,
            provider_request_id: None,
        }
    }
}

impl fmt::Debug for ModelErrorTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelErrorTarget")
            .field("protocol", &self.protocol)
            .field("route", &"<redacted>")
            .field("model", &"<redacted>")
            .finish()
    }
}

/// Safe provider-neutral context attached to a terminal model failure.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelErrorContext {
    protocol: ProtocolId,
    route: RouteId,
    model: ModelId,
    phase: ModelErrorPhase,
    retryability: ModelRetryability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    http_status: Option<ModelHttpStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_code: Option<ModelProviderCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_request_id: Option<ModelProviderRequestId>,
}

#[derive(Deserialize)]
struct ModelErrorContextWire {
    protocol: ProtocolId,
    route: RouteId,
    model: ModelId,
    phase: ModelErrorPhase,
    retryability: ModelRetryability,
    #[serde(default)]
    http_status: Option<ModelHttpStatus>,
    #[serde(default)]
    provider_code: Option<ModelProviderCode>,
    #[serde(default)]
    provider_request_id: Option<ModelProviderRequestId>,
}

impl<'de> Deserialize<'de> for ModelErrorContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelErrorContextWire::deserialize(deserializer)?;
        let mut context = Self::new(
            wire.protocol,
            wire.route,
            wire.model,
            wire.phase,
            wire.retryability,
        )
        .map_err(serde::de::Error::custom)?;
        context.http_status = wire.http_status;
        context.provider_code = wire.provider_code;
        context.provider_request_id = wire.provider_request_id;
        Ok(context)
    }
}

impl fmt::Debug for ModelErrorContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelErrorContext")
            .field("protocol", &self.protocol)
            .field("route", &"<redacted>")
            .field("model", &"<redacted>")
            .field("phase", &self.phase)
            .field("retryability", &self.retryability)
            .field("http_status", &self.http_status)
            .field(
                "provider_code",
                &self.provider_code.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "provider_request_id",
                &self.provider_request_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl ModelErrorContext {
    /// Creates context for an exact resolved protocol, route, and model.
    ///
    /// # Errors
    ///
    /// Returns an error when a target identity is credential-shaped or would
    /// change under the shared public redaction policy.
    pub fn new(
        protocol: ProtocolId,
        route: RouteId,
        model: ModelId,
        phase: ModelErrorPhase,
        retryability: ModelRetryability,
    ) -> Result<Self, ModelErrorContextError> {
        let target = ModelErrorTarget::new(protocol, route, model)?;
        Ok(Self {
            protocol: target.protocol,
            route: target.route,
            model: target.model,
            phase,
            retryability,
            http_status: None,
            provider_code: None,
            provider_request_id: None,
        })
    }

    /// Attaches a validated HTTP response status.
    #[must_use]
    pub const fn with_http_status(mut self, status: ModelHttpStatus) -> Self {
        self.http_status = Some(status);
        self
    }

    /// Attaches a bounded, validated provider error code.
    #[must_use]
    pub fn with_provider_code(mut self, code: ModelProviderCode) -> Self {
        self.provider_code = Some(code);
        self
    }

    /// Attaches a bounded, validated provider request identifier.
    #[must_use]
    pub fn with_provider_request_id(mut self, request_id: ModelProviderRequestId) -> Self {
        self.provider_request_id = Some(request_id);
        self
    }

    /// Returns the protocol-dialect identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the requested model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the operation phase.
    #[must_use]
    pub const fn phase(&self) -> ModelErrorPhase {
        self.phase
    }

    /// Returns the retry and duplicate-effect disposition.
    #[must_use]
    pub const fn retryability(&self) -> ModelRetryability {
        self.retryability
    }

    /// Returns the validated HTTP response status, when available.
    #[must_use]
    pub const fn http_status(&self) -> Option<ModelHttpStatus> {
        self.http_status
    }

    /// Returns the bounded provider error code, when available.
    #[must_use]
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_ref().map(ModelProviderCode::as_str)
    }

    /// Returns the bounded provider request identifier, when available.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id
            .as_ref()
            .map(ModelProviderRequestId::as_str)
    }
}

fn safe_target_identity(value: &str) -> bool {
    !value.to_ascii_lowercase().starts_with("bearer")
        && SafeDiagnostic::new(value).as_str() == value
}

/// Sanitized source retained without exposing a provider-specific error type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedactedModelErrorSource(SafeDiagnostic);

impl RedactedModelErrorSource {
    /// Sanitizes an untrusted lower-level source for retention.
    #[must_use]
    pub fn new(source: impl AsRef<str>) -> Self {
        Self(SafeDiagnostic::new(source))
    }

    /// Returns the bounded sanitized source text.
    #[must_use]
    pub fn message(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for RedactedModelErrorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for RedactedModelErrorSource {}

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

/// Bounded exact-projection capacity failure retained across terminal recovery.
///
/// `Compactable` means only that the checked history contains an eligible
/// semantic prefix. It does not predict savings, authorize a paid call, or
/// imply that compaction will make the retained projection fit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextCapacityFailure {
    /// Complete eligible history groups exist for an explicit compaction attempt.
    Compactable {
        /// Identity of the exact checked context status.
        status: Digest,
        /// Identity of the exact model-visible input that overflowed.
        model_input: Digest,
        /// Checked capacity-axis overflow.
        overflow: ContextOverflow,
    },
    /// The projection overflowed with no eligible history group to compact.
    Protected {
        /// Identity of the exact checked context status.
        status: Digest,
        /// Identity of the exact model-visible input that overflowed.
        model_input: Digest,
        /// Checked capacity-axis overflow.
        overflow: ContextOverflow,
    },
}

impl ContextCapacityFailure {
    fn from_status(status: &ContextStatus) -> Option<Self> {
        let common = (status.identity(), status.model_input_identity());
        match status.kind() {
            ContextStatusKind::WouldOverflow(overflow) => Some(Self::Compactable {
                status: common.0,
                model_input: common.1,
                overflow: *overflow,
            }),
            ContextStatusKind::ProtectedOverflow(overflow) => Some(Self::Protected {
                status: common.0,
                model_input: common.1,
                overflow: *overflow,
            }),
            ContextStatusKind::Fits { .. } | ContextStatusKind::Unavailable(_) => None,
        }
    }

    /// Returns the identity of the exact checked context status.
    #[must_use]
    pub const fn status_identity(self) -> Digest {
        match self {
            Self::Compactable { status, .. } | Self::Protected { status, .. } => status,
        }
    }

    /// Returns the exact model-visible input identity that overflowed.
    #[must_use]
    pub const fn model_input_identity(self) -> Digest {
        match self {
            Self::Compactable { model_input, .. } | Self::Protected { model_input, .. } => {
                model_input
            }
        }
    }

    /// Returns the checked capacity-axis overflow.
    #[must_use]
    pub const fn overflow(self) -> ContextOverflow {
        match self {
            Self::Compactable { overflow, .. } | Self::Protected { overflow, .. } => overflow,
        }
    }

    /// Reports whether explicit compaction may be attempted from the retained source.
    #[must_use]
    pub const fn is_compactable(self) -> bool {
        matches!(self, Self::Compactable { .. })
    }
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
    /// Errors crossing a resolved [`LanguageModel`](super::LanguageModel) boundary must instead use
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

    /// Creates an outcome-unknown, permanently non-retryable error with exact
    /// paid recovery evidence and caller-supplied immutable context.
    ///
    /// # Errors
    ///
    /// Returns an error unless the supplied context is permanently
    /// non-retryable.
    pub fn contextual_paid_recovery(
        message: impl Into<String>,
        context: ModelErrorContext,
        paid_recovery: ModelPaidRecovery,
    ) -> Result<Self, ModelErrorContextError> {
        let mut error = Self::contextual(ModelErrorKind::OutcomeUnknown, message, context)?;
        error.attach_paid_recovery(paid_recovery)?;
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

    pub(super) fn bind_dispatch_rejection(&mut self, prepared_call: Digest, attempt_id: AttemptId) {
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

/// Classifies a provider's HTTP rejection body as a context-window overflow.
///
/// Providers name the condition in their own words and only some send a code,
/// so kernels map a rejection to [`ModelErrorKind::ContextLimit`] when either
/// its code or its message matches one of the known phrasings. The prose is
/// never retained; only the classification is.
#[must_use]
pub fn provider_rejection_is_context_overflow(code: Option<&str>, message: &str) -> bool {
    const CODES: [&str; 2] = ["context_length_exceeded", "context_window_exceeded"];
    const PHRASES: [&str; 6] = [
        "prompt is too long",
        "maximum context length",
        "maximum prompt length",
        "context_length_exceeded",
        "context length exceeded",
        "exceeds the context window",
    ];
    if code.is_some_and(|code| CODES.contains(&code)) {
        return true;
    }
    let message = message.to_ascii_lowercase();
    PHRASES.iter().any(|phrase| message.contains(phrase))
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
