//! Structured, ordered, bounded tool outcomes.

use std::fmt;
use std::sync::Arc;

use plakous_model::{
    TextBlock, ToolEffect, ToolOutputContract, ToolResultBlock, ToolResultKind, ToolResultPart,
};

use crate::{ToolInvocation, ToolRuntimeError, ToolRuntimeErrorKind};

pub(crate) mod images;

pub(crate) use images::prepare_tool_images;

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

/// How a wait ended, as the waiting tool classified its own result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaitWake {
    /// Every waited-on target reached a terminal state.
    Settled,
    /// Mail for the waiter ended the wait.
    Mail,
    /// The deadline passed with a target still running.
    Timeout,
    /// Steering or a notice cut the wait short.
    Interrupted,
}

/// What one wait waited on and how it ended: counts and a closed label,
/// never an id, an output or a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WaitFact {
    /// Child agents the wait watched.
    pub agents: u32,
    /// Processes the wait watched.
    pub processes: u32,
    /// What ended it.
    pub wake: WaitWake,
}

impl WaitFact {
    /// One wait over `agents` children and `processes` processes.
    #[must_use]
    pub const fn new(agents: u32, processes: u32, wake: WaitWake) -> Self {
        Self {
            agents,
            processes,
            wake,
        }
    }
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
    pub(crate) images_prepared: bool,
    value: Option<Box<crate::ExecutionValue>>,
    kind: ToolOutcomeKind,
    parts: Vec<ToolResultPart>,
    retained_bytes: usize,
    process_session_update: Option<crate::internal::ProcessSessionUpdate>,
    pub(crate) process_telemetry: Option<crate::internal::ProcessTelemetry>,
    wait_fact: Option<WaitFact>,
    /// Bounded, redacted label for a host surface. Never part of [`Self::to_model_result`].
    presentation_subject: Option<Arc<str>>,
    /// Set by a raising program that renders a text file read, so an
    /// observation cut can keep it head-first.
    /// Boxed so the mark does not widen every outcome and every `Result` that
    /// carries one.
    file_read: Option<Box<crate::internal::RaisedFileRead>>,
}

impl fmt::Debug for ToolOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolOutcome")
            .field("images_prepared", &self.images_prepared)
            .field("kind", &self.kind)
            .field("part_count", &self.parts.len())
            .field("retained_bytes", &self.retained_bytes)
            .field("process_session_update", &self.process_session_update)
            .field("process_telemetry", &self.process_telemetry)
            .field("wait_fact", &self.wait_fact)
            .field("presentation_subject", &self.presentation_subject)
            .field("file_read", &self.file_read)
            .field("parts", &"<redacted>")
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ToolOutcome {
    /// Attaches an ephemeral value without changing canonical result parts.
    /// Executors must construct it only when the invocation requests it.
    ///
    /// # Errors
    ///
    /// Rejects a value whose retained allocation exceeds the default byte bound.
    pub fn with_value(mut self, value: crate::ExecutionValue) -> Result<Self, ToolRuntimeError> {
        if value.retained_bytes() > MAX_TOOL_OUTCOME_BYTES {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "typed execution value exceeds its byte limit",
            )
            .with_proven_no_effect(false));
        }
        self.value = Some(Box::new(value));
        Ok(self)
    }

    /// Moves the typed result out before presentation or evidence is copied.
    /// Values never enter canonical history, checkpoints, or replay.
    pub fn take_value(&mut self) -> Option<crate::ExecutionValue> {
        self.value.take().map(|value| *value)
    }

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

    /// Attaches what a wait waited on and how it ended, classified by the
    /// waiting tool from its own typed result.
    #[must_use]
    pub fn with_wait_fact(mut self, fact: WaitFact) -> Self {
        self.wait_fact = Some(fact);
        self
    }

    /// The wait fact a waiting tool attached, if any.
    #[must_use]
    pub const fn wait_fact(&self) -> Option<WaitFact> {
        self.wait_fact
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

    /// Marks this raised outcome as the file read its raising program rendered.
    #[must_use]
    pub(crate) fn into_file_read(mut self, read: crate::internal::RaisedFileRead) -> Self {
        self.file_read = Some(Box::new(read));
        self
    }

    pub(crate) fn file_read(&self) -> Option<&crate::internal::RaisedFileRead> {
        self.file_read.as_deref()
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
            value: None,
            kind,
            parts,
            retained_bytes,
            process_session_update: None,
            process_telemetry: None,
            wait_fact: None,
            presentation_subject: None,
            file_read: None,
            images_prepared: false,
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
                value: None,
                kind: ToolOutcomeKind::OutcomeUnknown,
                parts: Vec::new(),
                retained_bytes: 0,
                process_session_update: None,
                process_telemetry: None,
                wait_fact: None,
                presentation_subject: None,
                file_read: None,
                images_prepared: false,
            },
        }
    }

    fn output_contract_failure(
        process_session_update: Option<crate::internal::ProcessSessionUpdate>,
        effect: ToolEffect,
    ) -> Self {
        let (kind, message) = if effect == ToolEffect::ReadOnly {
            (
                ToolOutcomeKind::Failure,
                "tool executor output violated its registered contract",
            )
        } else {
            (
                ToolOutcomeKind::OutcomeUnknown,
                "tool completed but its output violated its contract; it may have acted",
            )
        };
        match Self::text(kind, message) {
            Ok(mut outcome) => {
                outcome.process_session_update = process_session_update;
                outcome
            }
            Err(_) => Self {
                value: None,
                kind,
                parts: Vec::new(),
                retained_bytes: 0,
                process_session_update,
                process_telemetry: None,
                wait_fact: None,
                presentation_subject: None,
                file_read: None,
                images_prepared: false,
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
        Self::raised_content(vec![ToolResultPart::Text(text)])
    }

    /// Retains canonical model content after a raised text envelope. An
    /// image-only canonical result needs one extra part for that envelope.
    pub(crate) fn raised_content(parts: Vec<ToolResultPart>) -> Result<Self, ToolRuntimeError> {
        Self::new(
            ToolOutcomeKind::Success,
            parts,
            ToolOutcomeLimits {
                max_parts: MAX_TOOL_OUTCOME_PARTS + 1,
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
        if self.retained_bytes > limits.max_bytes
            || self
                .value
                .as_ref()
                .is_some_and(|value| value.retained_bytes() > limits.max_bytes)
        {
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

    pub(crate) fn enforce_completed_output_contract(
        self,
        contract: &ToolOutputContract,
        effect: ToolEffect,
    ) -> Self {
        let process_session_update = self.process_session_update.clone();
        let process_telemetry = self.process_telemetry;
        let presentation_subject = self.presentation_subject.clone();
        self.validate_contract(contract).unwrap_or_else(|_| {
            let mut outcome = Self::output_contract_failure(process_session_update, effect);
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
mod tests;
