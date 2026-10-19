//! Structured, ordered, bounded tool outcomes.

use std::fmt;
use std::sync::Arc;

use plakous_model::{
    TextBlock, ToolOutputContract, ToolResultBlock, ToolResultKind, ToolResultPart,
};

use crate::{ToolInvocation, ToolRuntimeError, ToolRuntimeErrorKind};

/// Default maximum retained bytes across one tool outcome.
pub const MAX_TOOL_OUTCOME_BYTES: usize = 1024 * 1024;
/// Default maximum ordered parts across one tool outcome.
pub const MAX_TOOL_OUTCOME_PARTS: usize = 128;
/// Host-only presentation subject bound, the same 240 bytes as a tool-call subject.
const PRESENTATION_SUBJECT_BYTES: usize = 240;

/// Semantic terminal state of a local tool attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolOutcomeKind {
    /// Model-visible successful execution.
    Success,
    /// Model-visible failure produced by the executor.
    Failure,
    /// A non-transactional effect may have occurred and must not be replayed.
    OutcomeUnknown,
}

/// Hard retained-content limits for a terminal outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolOutcomeLimits {
    /// Maximum ordered canonical parts.
    pub max_parts: usize,
    /// Maximum encoded bytes across all parts.
    pub max_bytes: usize,
}

impl Default for ToolOutcomeLimits {
    fn default() -> Self {
        Self {
            max_parts: MAX_TOOL_OUTCOME_PARTS,
            max_bytes: MAX_TOOL_OUTCOME_BYTES,
        }
    }
}

impl ToolOutcomeLimits {
    pub(crate) fn validate(self) -> Result<Self, ToolRuntimeError> {
        if self.max_parts == 0 || self.max_bytes == 0 {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool outcome limits must be nonzero",
            ));
        }
        Ok(self)
    }
}

/// One terminal outcome retaining canonical result-part order.
#[derive(Clone, PartialEq)]
pub struct ToolOutcome {
    kind: ToolOutcomeKind,
    parts: Vec<ToolResultPart>,
    retained_bytes: usize,
    process_session_update: Option<crate::internal::ProcessSessionUpdate>,
    pub(crate) process_telemetry: Option<crate::internal::ProcessTelemetry>,
    /// Bounded, redacted label for a host surface. Never part of [`Self::to_model_result`].
    presentation_subject: Option<Arc<str>>,
}

impl fmt::Debug for ToolOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolOutcome")
            .field("kind", &self.kind)
            .field("part_count", &self.parts.len())
            .field("retained_bytes", &self.retained_bytes)
            .field("process_session_update", &self.process_session_update)
            .field("process_telemetry", &self.process_telemetry)
            .field("presentation_subject", &self.presentation_subject)
            .field("parts", &"<redacted>")
            .finish()
    }
}

impl ToolOutcome {
    /// Attaches a neutral await observation from the tool's own typed wake result.
    /// `yielded` means observation was cut short by steering, not cancellation.
    #[must_use]
    pub fn with_wait_observation(mut self, yielded: bool) -> Self {
        self.process_telemetry = Some(crate::internal::ProcessTelemetry::Waited {
            operation: crate::internal::ProcessOperationKind::Await,
            yielded,
        });
        self
    }

    /// Attaches a host-only presentation subject.
    ///
    /// The text is redacted, forced onto one line, and bounded to
    /// 240 bytes with one trailing ellipsis. Empty input
    /// clears it. It is not part of the canonical model result.
    #[must_use]
    pub fn with_presentation_subject(mut self, subject: impl AsRef<str>) -> Self {
        self.presentation_subject = bound_presentation_subject(subject.as_ref());
        self
    }

    /// The host-only presentation subject, when one was attached.
    #[must_use]
    pub fn presentation_subject(&self) -> Option<&str> {
        self.presentation_subject.as_deref()
    }

    /// Creates a bounded terminal outcome from canonical ordered result parts.
    ///
    /// # Errors
    ///
    /// Returns a resource error when content is empty or exceeds configured limits.
    pub fn new(
        kind: ToolOutcomeKind,
        parts: Vec<ToolResultPart>,
        limits: ToolOutcomeLimits,
    ) -> Result<Self, ToolRuntimeError> {
        if parts.is_empty() {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "tool outcome must contain at least one result part",
            ));
        }
        if parts.len() > limits.max_parts {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool outcome exceeds its part-count limit",
            )
            .with_proven_no_effect(false));
        }
        let mut retained_bytes = 0_usize;
        for part in &parts {
            let bytes = serde_json::to_vec(part).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "tool outcome could not be measured",
                    &error,
                )
            })?;
            retained_bytes = retained_bytes.saturating_add(bytes.len());
            if retained_bytes > limits.max_bytes {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "tool outcome exceeds its byte limit",
                )
                .with_proven_no_effect(false));
            }
        }
        Ok(Self {
            kind,
            parts,
            retained_bytes,
            process_session_update: None,
            process_telemetry: None,
            presentation_subject: None,
        })
    }

    /// Creates a success using default hard limits.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized canonical content.
    pub fn success(parts: Vec<ToolResultPart>) -> Result<Self, ToolRuntimeError> {
        Self::new(
            ToolOutcomeKind::Success,
            parts,
            ToolOutcomeLimits::default(),
        )
    }

    /// Creates a model-visible failure using default hard limits.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized canonical content.
    pub fn failure(parts: Vec<ToolResultPart>) -> Result<Self, ToolRuntimeError> {
        Self::new(
            ToolOutcomeKind::Failure,
            parts,
            ToolOutcomeLimits::default(),
        )
    }

    /// Creates an indeterminate-effect result using default hard limits.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized canonical content.
    pub fn outcome_unknown(parts: Vec<ToolResultPart>) -> Result<Self, ToolRuntimeError> {
        Self::new(
            ToolOutcomeKind::OutcomeUnknown,
            parts,
            ToolOutcomeLimits::default(),
        )
    }

    /// Creates a single-text indeterminate-effect outcome.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized text.
    pub fn outcome_unknown_text(text: impl Into<String>) -> Result<Self, ToolRuntimeError> {
        Self::text(ToolOutcomeKind::OutcomeUnknown, text)
    }

    pub(crate) fn runtime_outcome_unknown() -> Self {
        match Self::outcome_unknown_text(
            "effect dispatch ended without an authoritative executor outcome",
        ) {
            Ok(outcome) => outcome,
            Err(_) => Self {
                kind: ToolOutcomeKind::OutcomeUnknown,
                parts: Vec::new(),
                retained_bytes: 0,
                process_session_update: None,
                process_telemetry: None,
                presentation_subject: None,
            },
        }
    }

    fn output_contract_failure(
        process_session_update: Option<crate::internal::ProcessSessionUpdate>,
    ) -> Self {
        match Self::failure_text("tool executor output violated its registered contract") {
            Ok(mut outcome) => {
                outcome.process_session_update = process_session_update;
                outcome
            }
            Err(_) => Self {
                kind: ToolOutcomeKind::Failure,
                parts: Vec::new(),
                retained_bytes: 0,
                process_session_update,
                process_telemetry: None,
                presentation_subject: None,
            },
        }
    }

    /// Creates a single-text success.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized text.
    pub fn success_text(text: impl Into<String>) -> Result<Self, ToolRuntimeError> {
        Self::text(ToolOutcomeKind::Success, text)
    }

    /// Creates the single-text success a projection raises from a canonical
    /// outcome that already satisfied its output contract.
    ///
    /// Wrapping that outcome's text in one presentation JSON object escapes it
    /// once more, which at most doubles its measured encoding, and a read
    /// presentation adds bounded line prefixes and notes; four times the
    /// default limit holds both. The runtime cuts the raised text to the
    /// per-result model-visible bound before it enters history.
    pub(crate) fn raised_text(text: String) -> Result<Self, ToolRuntimeError> {
        let text = TextBlock::new(text, None).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "raised tool text outcome is invalid",
                &error,
            )
        })?;
        Self::new(
            ToolOutcomeKind::Success,
            vec![ToolResultPart::Text(text)],
            ToolOutcomeLimits {
                max_parts: 1,
                max_bytes: 4 * MAX_TOOL_OUTCOME_BYTES,
            },
        )
    }

    /// Creates a single-text model-visible failure.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized text.
    pub fn failure_text(text: impl Into<String>) -> Result<Self, ToolRuntimeError> {
        Self::text(ToolOutcomeKind::Failure, text)
    }

    /// Returns the terminal semantic kind.
    #[must_use]
    pub const fn kind(&self) -> ToolOutcomeKind {
        self.kind
    }

    /// Returns whether this outcome is a successful tool result.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self.kind, ToolOutcomeKind::Success)
    }

    /// Concatenates text result parts in order. Non-text parts are omitted.
    #[must_use]
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                ToolResultPart::Text(block) => Some(block.text()),
                _ => None,
            })
            .collect()
    }

    /// Returns ordered canonical result parts.
    #[must_use]
    pub fn parts(&self) -> &[ToolResultPart] {
        &self.parts
    }

    /// Returns measured retained bytes.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub(crate) fn with_process_session_update(
        mut self,
        update: crate::internal::ProcessSessionUpdate,
    ) -> Self {
        self.process_session_update = Some(update);
        self
    }

    pub(crate) fn process_session_update(&self) -> Option<crate::internal::ProcessSessionUpdate> {
        self.process_session_update.clone()
    }

    pub(crate) fn enforce_limits(
        self,
        limits: ToolOutcomeLimits,
    ) -> Result<Self, ToolRuntimeError> {
        if self.parts.len() > limits.max_parts {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool outcome exceeds the runtime part-count limit",
            )
            .with_proven_no_effect(false));
        }
        if self.retained_bytes > limits.max_bytes {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool outcome exceeds the runtime byte limit",
            )
            .with_proven_no_effect(false));
        }
        Ok(self)
    }

    /// Validates successful semantic output before it can become model-visible.
    pub(crate) fn validate_contract(
        self,
        contract: &ToolOutputContract,
    ) -> Result<Self, ToolRuntimeError> {
        if self.kind != ToolOutcomeKind::Success {
            return Ok(self);
        }
        let valid = match (contract, self.parts.as_slice()) {
            (ToolOutputContract::ModelContent(limit), _) => {
                self.parts.len() <= limit.max_parts() as usize
                    && self.retained_bytes <= limit.max_bytes() as usize
            }
            (ToolOutputContract::Utf8(limit), [ToolResultPart::Text(text)]) => {
                limit.validate(text.text()).is_ok()
            }
            (ToolOutputContract::Structured(schema), [ToolResultPart::Text(text)]) => {
                schema.parse_instance(text.text()).is_ok()
            }
            _ => false,
        };
        if !valid {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "successful tool output violates its registered contract",
            ));
        }
        Ok(self)
    }

    pub(crate) fn enforce_completed_output_contract(self, contract: &ToolOutputContract) -> Self {
        let process_session_update = self.process_session_update.clone();
        let process_telemetry = self.process_telemetry;
        let presentation_subject = self.presentation_subject.clone();
        self.validate_contract(contract).unwrap_or_else(|_| {
            let mut outcome = Self::output_contract_failure(process_session_update);
            outcome.process_telemetry = process_telemetry;
            outcome.presentation_subject = presentation_subject;
            outcome
        })
    }

    /// Converts the outcome to the canonical model result for its invocation,
    /// owned by the invoked registration's [`crate::ToolOwnerClass`].
    ///
    /// `OutcomeUnknown` is model-visible failure content while its stronger
    /// replay prohibition remains available through [`Self::kind`].
    ///
    /// # Errors
    ///
    /// Returns an execution error only if canonical result construction fails.
    pub fn to_model_result(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ToolResultBlock, ToolRuntimeError> {
        let kind = match self.kind {
            ToolOutcomeKind::Success => ToolResultKind::Success,
            ToolOutcomeKind::Failure | ToolOutcomeKind::OutcomeUnknown => ToolResultKind::Failure,
        };
        ToolResultBlock::new(
            invocation.call_id(),
            invocation.identity().owner().execution_owner(),
            kind,
            self.parts.clone(),
            invocation.correlation().clone(),
        )
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "canonical tool result could not be constructed",
                &error,
            )
        })
    }

    fn text(kind: ToolOutcomeKind, text: impl Into<String>) -> Result<Self, ToolRuntimeError> {
        let text = TextBlock::new(text, None).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "tool text outcome is invalid",
                &error,
            )
        })?;
        Self::new(
            kind,
            vec![ToolResultPart::Text(text)],
            ToolOutcomeLimits::default(),
        )
    }
}

/// Redacts, flattens to one line, and bounds a host presentation subject.
fn bound_presentation_subject(raw: &str) -> Option<Arc<str>> {
    let redacted = crate::internal::preview::text(raw.trim());
    let single = redacted.replace("\r\n", "↵").replace(['\r', '\n'], "↵");
    let single = single.trim();
    if single.is_empty() {
        return None;
    }
    if single.len() <= PRESENTATION_SUBJECT_BYTES {
        return Some(Arc::from(single));
    }
    let ellipsis = "…";
    let mut end = PRESENTATION_SUBJECT_BYTES - ellipsis.len();
    while !single.is_char_boundary(end) {
        end -= 1;
    }
    Some(Arc::from(format!("{}{ellipsis}", &single[..end])))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    fn structured_output() -> Result<ToolOutputContract, Box<dyn std::error::Error>> {
        Ok(ToolOutputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }))?)
    }

    #[test]
    fn outcome_bounds_are_enforced_without_flattening_order() -> Result<(), ToolRuntimeError> {
        let parts = vec![
            ToolResultPart::Text(TextBlock::new("first", None).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "test text failed",
                    &error,
                )
            })?),
            ToolResultPart::Text(TextBlock::new("second", None).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "test text failed",
                    &error,
                )
            })?),
        ];
        let outcome = ToolOutcome::success(parts.clone())?;
        assert_eq!(outcome.parts(), parts);
        assert!(outcome.is_success());
        assert_eq!(outcome.joined_text(), "firstsecond");
        assert!(
            ToolOutcome::new(
                ToolOutcomeKind::Success,
                parts,
                ToolOutcomeLimits {
                    max_parts: 1,
                    max_bytes: 1024
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn successful_output_contracts_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        ToolOutcome::success_text(r#"{"answer":"yes"}"#)?
            .validate_contract(&structured_output()?)?;
        assert!(
            ToolOutcome::success_text(r#"{"answer":1}"#)?
                .validate_contract(&structured_output()?)
                .is_err()
        );

        let utf8 = ToolOutputContract::utf8(4)?;
        ToolOutcome::success_text("four")?.validate_contract(&utf8)?;
        assert!(
            ToolOutcome::success_text("five!")?
                .validate_contract(&utf8)
                .is_err()
        );

        // Failure content reports the failure and is not reclassified as a
        // successful semantic output.
        ToolOutcome::failure_text("bounded failure")?.validate_contract(&structured_output()?)?;
        Ok(())
    }

    #[test]
    fn output_contract_failure_preserves_process_lifecycle_truth()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::num::NonZeroU64;

        for update in [
            crate::internal::ProcessSessionUpdate::Active {
                session_id: [1; 16],
                owner_generation: NonZeroU64::new(1).ok_or("generation")?,
            },
            crate::internal::ProcessSessionUpdate::Joined {
                session_id: [2; 16],
                owner_generation: NonZeroU64::new(2).ok_or("generation")?,
            },
        ] {
            let outcome = ToolOutcome::success_text("too long")?
                .with_process_session_update(update.clone())
                .enforce_completed_output_contract(&ToolOutputContract::utf8(1)?);
            assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
            assert_eq!(outcome.process_session_update(), Some(update));
        }
        Ok(())
    }

    #[test]
    fn model_content_contract_enforces_exact_part_and_encoded_byte_limits()
    -> Result<(), Box<dyn std::error::Error>> {
        let one = ToolOutcome::success_text("four")?;
        let encoded_bytes = u32::try_from(one.retained_bytes())?;
        assert_eq!(
            one.clone()
                .enforce_completed_output_contract(&ToolOutputContract::model_content(
                    1,
                    encoded_bytes,
                )?)
                .kind(),
            ToolOutcomeKind::Success
        );
        assert_eq!(
            one.clone()
                .enforce_completed_output_contract(&ToolOutputContract::model_content(
                    1,
                    encoded_bytes - 1,
                )?)
                .kind(),
            ToolOutcomeKind::Failure
        );

        let two = ToolOutcome::success(vec![
            ToolResultPart::Text(TextBlock::new("a", None)?),
            ToolResultPart::Text(TextBlock::new("b", None)?),
        ])?;
        assert_eq!(
            two.enforce_completed_output_contract(&ToolOutputContract::model_content(
                1,
                plakous_model::ModelContentContractV1::MAX_BYTES,
            )?)
            .kind(),
            ToolOutcomeKind::Failure
        );
        Ok(())
    }
}
