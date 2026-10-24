//! Portable request policies, request metadata and native options.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    AttemptId, ModelOperationRoot, NamespaceId, PortableToolSchemaV1, RunId, SessionWorkflowRootId,
    Timestamp, ToolName, TurnId,
};

use super::{RequestError, ensure_nonempty};

/// The source of an ordered instruction block.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum InstructionSource {
    /// A system instruction.
    System,
    /// A developer instruction.
    Developer,
    /// A frozen harness-profile instruction.
    Profile,
    /// A harness-generated operational instruction.
    Harness,
}

/// One ordered instruction kept separate from conversation history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstructionBlock {
    source: InstructionSource,
    text: String,
}

#[derive(Deserialize)]
struct InstructionBlockWire {
    source: InstructionSource,
    text: String,
}

impl<'de> Deserialize<'de> for InstructionBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstructionBlockWire::deserialize(deserializer)?;
        Self::new(wire.source, wire.text).map_err(serde::de::Error::custom)
    }
}

impl InstructionBlock {
    /// Creates a non-empty instruction.
    ///
    /// # Errors
    ///
    /// Returns an error when instruction text is empty or exceeds its portable budget.
    pub fn new(source: InstructionSource, text: impl Into<String>) -> Result<Self, RequestError> {
        let text = text.into();
        ensure_nonempty(&text, "instruction")?;
        if text.len() > 65_536 {
            return Err(RequestError::TooLong {
                field: "instruction",
                maximum: 65_536,
            });
        }
        Ok(Self { source, text })
    }

    /// Returns instruction provenance.
    #[must_use]
    pub const fn source(&self) -> InstructionSource {
        self.source
    }

    /// Returns instruction text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Whether failure to support an intent is fatal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CapabilityRequirement {
    /// The request must fail if the intent cannot be satisfied.
    Required,
    /// The intent may be explicitly downgraded when policy permits.
    Preferred,
}

/// A portable requested output format.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OutputFormat {
    /// Plain text output.
    Text,
    /// JSON output constrained by the supplied schema.
    Structured(PortableToolSchemaV1),
}

impl OutputFormat {
    /// Builds a locally enforceable structured assistant-output format.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is outside the portable V1 subset.
    pub fn structured(schema: Value) -> Result<Self, RequestError> {
        Ok(Self::Structured(PortableToolSchemaV1::new(schema)?))
    }
}

/// Portable output intent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputPolicy {
    format: OutputFormat,
    requirement: CapabilityRequirement,
}

#[derive(Deserialize)]
struct OutputPolicyWire {
    format: OutputFormat,
    requirement: CapabilityRequirement,
}

impl<'de> Deserialize<'de> for OutputPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OutputPolicyWire::deserialize(deserializer)?;
        Self::new(wire.format, wire.requirement).map_err(serde::de::Error::custom)
    }
}

impl OutputPolicy {
    /// Creates an output policy, validating a structured-output schema.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::InvalidSchema`] for an unsupported structured schema.
    pub fn new(
        format: OutputFormat,
        requirement: CapabilityRequirement,
    ) -> Result<Self, RequestError> {
        Ok(Self {
            format,
            requirement,
        })
    }

    /// Returns the requested format.
    #[must_use]
    pub const fn format(&self) -> &OutputFormat {
        &self.format
    }

    /// Returns the support requirement.
    #[must_use]
    pub const fn requirement(&self) -> CapabilityRequirement {
        self.requirement
    }
}

impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            requirement: CapabilityRequirement::Required,
        }
    }
}

/// A portable reasoning effort.
///
/// Dialects map only the levels they admit for a given model and fail closed on
/// the rest. Labels align with common Responses-family wire strings where
/// applicable (`low` … `xhigh` / `max` / `ultra`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReasoningEffort {
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high effort (Responses-family `xhigh` where admitted).
    XHigh,
    /// Maximum effort (Responses-family `max` where admitted, e.g. some 5.6 models).
    Max,
    /// Ultra effort (Responses-family `ultra` where admitted, e.g. sol/terra).
    Ultra,
    /// Explicitly no reasoning (`none`). Distinct from an omitted effort, which
    /// stays refused: a caller has to name this level.
    None,
    /// A provider-neutral token budget.
    TokenBudget(NonZeroU32),
}

/// Portable reasoning intent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReasoningPolicy {
    /// Do not request reasoning controls.
    #[default]
    Disabled,
    /// Request reasoning support.
    Enabled {
        /// The desired effort when specified.
        effort: Option<ReasoningEffort>,
        /// Whether support is required or preferred.
        requirement: CapabilityRequirement,
    },
}

/// Portable tool-selection intent.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolChoice {
    /// Let the model decide whether to call a tool.
    #[default]
    Auto,
    /// Disable tool calls for this request.
    None,
    /// Require at least one tool call.
    Required,
    /// Require one named exposed tool.
    Specific(ToolName),
}

/// Portable prompt-cache intent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CachePolicy {
    /// Do not request cache behavior.
    #[default]
    Disabled,
    /// Request cache behavior.
    Enabled {
        /// Whether support is required or preferred.
        requirement: CapabilityRequirement,
    },
}

/// Portable generation limits.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GenerationLimits {
    max_output_tokens: Option<NonZeroU32>,
    stop_sequences: Vec<String>,
}

#[derive(Deserialize)]
struct GenerationLimitsWire {
    max_output_tokens: Option<NonZeroU32>,
    stop_sequences: Vec<String>,
}

impl<'de> Deserialize<'de> for GenerationLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GenerationLimitsWire::deserialize(deserializer)?;
        Self::new(wire.max_output_tokens, wire.stop_sequences).map_err(serde::de::Error::custom)
    }
}

impl GenerationLimits {
    /// Creates validated portable limits.
    ///
    /// # Errors
    ///
    /// Returns an error for more than 16 stop sequences or any empty sequence.
    pub fn new(
        max_output_tokens: Option<NonZeroU32>,
        stop_sequences: Vec<String>,
    ) -> Result<Self, RequestError> {
        if stop_sequences.len() > 16 {
            return Err(RequestError::TooManyStopSequences);
        }
        if stop_sequences.iter().any(String::is_empty) {
            return Err(RequestError::Empty {
                field: "stop sequence",
            });
        }
        if stop_sequences.iter().any(|sequence| sequence.len() > 4_096) {
            return Err(RequestError::TooLong {
                field: "stop sequence",
                maximum: 4_096,
            });
        }
        Ok(Self {
            max_output_tokens,
            stop_sequences,
        })
    }

    /// Returns the optional output-token limit.
    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<NonZeroU32> {
        self.max_output_tokens
    }

    /// Returns ordered stop sequences.
    #[must_use]
    pub fn stop_sequences(&self) -> &[String] {
        &self.stop_sequences
    }
}

/// Stable metadata carried with a model request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestMetadata {
    run_id: RunId,
    root: ModelOperationRoot,
    attempt_id: AttemptId,
    created_at: Timestamp,
}

impl RequestMetadata {
    /// Creates metadata for one ordinary root turn.
    #[must_use]
    pub const fn for_turn(
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        created_at: Timestamp,
    ) -> Self {
        Self {
            run_id,
            root: ModelOperationRoot::Turn(turn_id),
            attempt_id,
            created_at,
        }
    }

    /// Creates metadata for one exact high-level session workflow.
    #[must_use]
    pub const fn for_session_workflow(
        run_id: RunId,
        workflow_id: SessionWorkflowRootId,
        attempt_id: AttemptId,
        created_at: Timestamp,
    ) -> Self {
        Self {
            run_id,
            root: ModelOperationRoot::SessionWorkflow(workflow_id),
            attempt_id,
            created_at,
        }
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(self) -> RunId {
        self.run_id
    }

    /// Returns the exact closed model-operation root.
    #[must_use]
    pub const fn root(self) -> ModelOperationRoot {
        self.root
    }

    /// Returns the turn identity only for an ordinary turn-rooted operation.
    #[must_use]
    pub const fn turn_id(self) -> Option<TurnId> {
        self.root.turn_id()
    }

    /// Returns the attempt identity.
    #[must_use]
    pub const fn attempt_id(self) -> AttemptId {
        self.attempt_id
    }

    /// Returns request creation time.
    #[must_use]
    pub const fn created_at(self) -> Timestamp {
        self.created_at
    }
}

/// Advanced namespaced options kept outside portable policy.
///
/// Numbers are kept as the request identity spells them (`1.0` becomes `1`,
/// `1e2` becomes `100`), so two options that compare unequal never share a
/// request identity.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(from = "NativeOptionsWire")]
pub struct NativeOptions {
    entries: BTreeMap<NamespaceId, Value>,
}

#[derive(Deserialize)]
struct NativeOptionsWire {
    entries: BTreeMap<NamespaceId, Value>,
}

impl From<NativeOptionsWire> for NativeOptions {
    fn from(wire: NativeOptionsWire) -> Self {
        Self::new(wire.entries)
    }
}

impl fmt::Debug for NativeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeOptions")
            .field("namespaces", &self.entries.keys().collect::<Vec<_>>())
            .field("values", &"<redacted>")
            .finish()
    }
}

impl NativeOptions {
    /// Creates options from namespace-keyed JSON values, numbers in their
    /// canonical spelling.
    #[must_use]
    pub fn new(entries: BTreeMap<NamespaceId, Value>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(namespace, value)| {
                    (
                        namespace,
                        crate::semantic_json::with_canonical_numbers(value),
                    )
                })
                .collect(),
        }
    }

    /// Returns immutable namespace-keyed values.
    #[must_use]
    pub const fn entries(&self) -> &BTreeMap<NamespaceId, Value> {
        &self.entries
    }
}
