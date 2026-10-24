//! Canonical tool definitions, portable policies, and owned model requests.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::mem::size_of;
use std::num::NonZeroU32;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::semantic_json::canonicalize_normalized_prevalidated;
use crate::{
    AttemptId, ConfiguredTool, ContextAdmissionRequirement, ConversationView, Digest, DigestDomain,
    DigestError, EffectiveProviderConfigurationId, HistoryError, HostedTool, ModelOperationRoot,
    NamespaceId, PortableToolSchemaV1, PortableToolSchemaV2, ProcessingTierDisposition, RunId,
    SelectedProviderConfigurationId, SemanticJsonError, SessionWorkflowRootId, Timestamp,
    ToolCallerDisposition, ToolExecutionOwner, ToolName, ToolResourceKey, TurnId,
};

/// An error constructing a canonical request value.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RequestError {
    /// A required field was empty.
    #[error("{field} cannot be empty")]
    Empty {
        /// The semantic field that was empty.
        field: &'static str,
    },
    /// A text field exceeded its portable byte budget.
    #[error("{field} exceeds {maximum} bytes")]
    TooLong {
        /// The semantic field that exceeded its budget.
        field: &'static str,
        /// The maximum accepted size.
        maximum: usize,
    },
    /// A JSON Schema value was invalid or outside the minimal subset.
    #[error("invalid JSON Schema: {reason}")]
    InvalidSchema {
        /// A stable human-readable reason.
        reason: String,
    },
    /// Semantic JSON or portable schema validation failed.
    #[error(transparent)]
    SemanticJson(#[from] SemanticJsonError),
    /// A request contained duplicate tool names.
    #[error("duplicate tool name: {0}")]
    DuplicateTool(ToolName),
    /// A specific tool choice did not name an exposed tool.
    #[error("selected tool is not exposed: {0}")]
    UnknownTool(ToolName),
    /// Tool use was required but the request exposed no tools.
    #[error("tool use cannot be required without an exposed tool")]
    RequiredToolWithoutTools,
    /// Too many stop sequences were supplied.
    #[error("at most 16 stop sequences are portable")]
    TooManyStopSequences,
    /// Too many ordered instructions were supplied.
    #[error("at most 256 instruction blocks are portable")]
    TooManyInstructions,
    /// Too many model-facing tools were supplied.
    #[error("at most 128 tool definitions are portable")]
    TooManyTools,
    /// Hosted-tool selection is duplicated or disagrees with configured caller dispositions.
    #[error("provider-hosted tool selection is invalid")]
    InvalidHostedTools,
    /// Too many native-option namespaces were supplied.
    #[error("at most 64 native-option namespaces are accepted")]
    TooManyNativeOptions,
    /// A JSON Schema exceeded the supported structural depth.
    #[error("JSON Schema exceeds the maximum depth of 64")]
    SchemaTooDeep,
    /// Canonical JSON serialization failed.
    #[error("canonical JSON serialization failed: {reason}")]
    CanonicalJson {
        /// The serialization failure.
        reason: String,
    },
    /// A digest could not be computed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// A serialized integrity digest did not match reconstructed content.
    #[error("serialized {field} digest does not match its content")]
    IntegrityMismatch {
        /// The semantic value whose integrity check failed.
        field: &'static str,
    },
    /// Tool concurrency metadata was empty, duplicated, or oversized.
    #[error("invalid tool concurrency metadata: {reason}")]
    InvalidToolConcurrency {
        /// Stable reason suitable for configuration diagnostics.
        reason: &'static str,
    },
    /// Canonical history or its bounded projection was invalid.
    #[error(transparent)]
    History(#[from] HistoryError),
    /// A supplied resolved harness-plan identity used the wrong digest domain.
    #[error("resolved harness-plan identity is invalid")]
    InvalidHarnessPlanIdentity,
    /// A compiled V2 tool projection was malformed or did not match the request binding.
    #[error("invalid compiled tool projection: {reason}")]
    InvalidToolProjection {
        /// Stable content-free reason.
        reason: &'static str,
    },
}

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

/// Bounded UTF-8 input or output contract, distinct from JSON schema.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct Utf8TextContractV1 {
    max_bytes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Utf8TextContractV1Wire {
    max_bytes: u32,
}

impl<'de> Deserialize<'de> for Utf8TextContractV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = Utf8TextContractV1Wire::deserialize(deserializer)?;
        Self::new(wire.max_bytes).map_err(serde::de::Error::custom)
    }
}

impl Utf8TextContractV1 {
    /// Creates a nonzero bounded UTF-8 contract.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::TooLong`] when the bound is zero or exceeds the
    /// format's fixed one-mebibyte ceiling.
    pub fn new(max_bytes: u32) -> Result<Self, RequestError> {
        if max_bytes == 0 || max_bytes > 1_048_576 {
            return Err(RequestError::TooLong {
                field: "UTF-8 contract",
                maximum: 1_048_576,
            });
        }
        Ok(Self { max_bytes })
    }
    /// Returns the exact byte limit.
    #[must_use]
    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }
    /// Validates one text value.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::TooLong`] when the UTF-8 byte length exceeds
    /// the declared bound.
    pub fn validate(self, text: &str) -> Result<(), RequestError> {
        if text.len() > self.max_bytes as usize {
            return Err(RequestError::TooLong {
                field: "UTF-8 value",
                maximum: self.max_bytes as usize,
            });
        }
        Ok(())
    }
}

/// Explicit local tool input meaning. Schema shape never selects a wire form.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolInputContract {
    /// Strict semantic JSON validated by `PortableToolSchemaV2`.
    Structured(PortableToolSchemaV2),
    /// Raw bounded UTF-8 text for an explicitly free-form tool.
    Utf8(Utf8TextContractV1),
}

impl ToolInputContract {
    /// Builds a structured contract through the sole V2 schema authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is outside the portable V2 subset.
    pub fn structured(schema: Value) -> Result<Self, RequestError> {
        Ok(Self::Structured(PortableToolSchemaV2::new(schema)?))
    }
    /// Builds an explicit free-form UTF-8 contract.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_bytes` is zero or exceeds the fixed bound.
    pub fn utf8(max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::Utf8(Utf8TextContractV1::new(max_bytes)?))
    }
}

/// Bounded ordered model-content output contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ModelContentContractV1 {
    max_parts: u32,
    max_bytes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelContentContractV1Wire {
    max_parts: u32,
    max_bytes: u32,
}

impl<'de> Deserialize<'de> for ModelContentContractV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelContentContractV1Wire::deserialize(deserializer)?;
        Self::new(wire.max_parts, wire.max_bytes).map_err(serde::de::Error::custom)
    }
}

impl ModelContentContractV1 {
    /// Fixed V1 ceiling for ordered model-content parts.
    pub const MAX_PARTS: u32 = 128;
    /// Fixed V1 ceiling for encoded model-content bytes.
    pub const MAX_BYTES: u32 = 1_048_576;

    /// Creates a nonzero bounded model-content contract.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero or exceeds its fixed V1 ceiling.
    pub fn new(max_parts: u32, max_bytes: u32) -> Result<Self, RequestError> {
        if max_parts == 0 || max_parts > Self::MAX_PARTS {
            return Err(RequestError::TooLong {
                field: "model-content part contract",
                maximum: Self::MAX_PARTS as usize,
            });
        }
        if max_bytes == 0 || max_bytes > Self::MAX_BYTES {
            return Err(RequestError::TooLong {
                field: "model-content byte contract",
                maximum: Self::MAX_BYTES as usize,
            });
        }
        Ok(Self {
            max_parts,
            max_bytes,
        })
    }

    /// Returns the exact ordered-part limit.
    #[must_use]
    pub const fn max_parts(self) -> u32 {
        self.max_parts
    }

    /// Returns the exact encoded-byte limit.
    #[must_use]
    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }
}

/// Explicit local tool output meaning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolOutputContract {
    /// Strict semantic JSON validated by `PortableToolSchemaV1`.
    Structured(PortableToolSchemaV1),
    /// Exactly one bounded UTF-8 text result.
    Utf8(Utf8TextContractV1),
    /// Bounded ordered model-content parts.
    ModelContent(ModelContentContractV1),
}

impl ToolOutputContract {
    /// Builds a structured output contract through the sole V1 schema authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is outside the portable V1 subset.
    pub fn structured(schema: Value) -> Result<Self, RequestError> {
        Ok(Self::Structured(PortableToolSchemaV1::new(schema)?))
    }
    /// Builds an explicit UTF-8 output contract.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_bytes` is zero or exceeds the fixed bound.
    pub fn utf8(max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::Utf8(Utf8TextContractV1::new(max_bytes)?))
    }
    /// Builds an explicit ordered model-content output contract.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero or exceeds its fixed V1 ceiling.
    pub fn model_content(max_parts: u32, max_bytes: u32) -> Result<Self, RequestError> {
        Ok(Self::ModelContent(ModelContentContractV1::new(
            max_parts, max_bytes,
        )?))
    }

    /// Builds the explicit portable-maximum model-content contract.
    #[must_use]
    pub const fn portable_model_content() -> Self {
        Self::ModelContent(ModelContentContractV1 {
            max_parts: ModelContentContractV1::MAX_PARTS,
            max_bytes: ModelContentContractV1::MAX_BYTES,
        })
    }
}

/// The scheduler concurrency class declared by a tool definition.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolConcurrency {
    /// Calls must be serialized with other effectful work.
    Serial,
    /// Independent calls may execute concurrently.
    Parallel,
    /// Calls exclude every other call naming any of the same resource keys.
    ResourceExclusive(Vec<ToolResourceKey>),
}

impl ToolConcurrency {
    const MAX_RESOURCE_KEYS: usize = 16;

    fn normalize(&mut self) -> Result<(), RequestError> {
        let Self::ResourceExclusive(keys) = self else {
            return Ok(());
        };
        if keys.is_empty() {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency requires at least one key",
            });
        }
        if keys.len() > Self::MAX_RESOURCE_KEYS {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency exceeds 16 keys",
            });
        }
        keys.sort();
        if keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RequestError::InvalidToolConcurrency {
                reason: "resource-exclusive concurrency contains a duplicate key",
            });
        }
        Ok(())
    }
}

/// The side-effect class declared by a tool definition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ToolEffect {
    /// The tool is declared read-only.
    ReadOnly,
    /// The tool may mutate state or affect external systems.
    Mutating,
}

/// An immutable canonical model-facing tool definition.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolDefinition {
    name: ToolName,
    semantic_version: String,
    description: String,
    input: ToolInputContract,
    output: ToolOutputContract,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
    digest: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolDefinitionWire {
    name: ToolName,
    semantic_version: String,
    description: String,
    input: ToolInputContract,
    output: ToolOutputContract,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
    digest: Digest,
}

impl<'de> Deserialize<'de> for ToolDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolDefinitionWire::deserialize(deserializer)?;
        let definition = Self::new(
            wire.name,
            wire.semantic_version,
            wire.description,
            wire.input,
            wire.output,
            wire.concurrency,
            wire.effect,
        )
        .map_err(serde::de::Error::custom)?;
        if definition.digest != wire.digest {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "tool definition",
            }));
        }
        Ok(definition)
    }
}

impl ToolDefinition {
    /// Creates and integrity-digests a validated tool definition.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized metadata, unsupported schema
    /// structure, canonicalization failure, or digest construction failure.
    pub fn new(
        name: ToolName,
        semantic_version: impl Into<String>,
        description: impl Into<String>,
        input: ToolInputContract,
        output: ToolOutputContract,
        mut concurrency: ToolConcurrency,
        effect: ToolEffect,
    ) -> Result<Self, RequestError> {
        let semantic_version = semantic_version.into();
        let description = description.into();
        ensure_nonempty(&semantic_version, "tool semantic version")?;
        ensure_nonempty(&description, "tool description")?;
        if semantic_version.len() > 128 {
            return Err(RequestError::TooLong {
                field: "tool semantic version",
                maximum: 128,
            });
        }
        if description.len() > 4_096 {
            return Err(RequestError::TooLong {
                field: "tool description",
                maximum: 4_096,
            });
        }
        let input_identity = tool_input_identity(&input);
        let output_identity = tool_output_identity(&output);
        concurrency.normalize()?;
        let concurrency_text = match &concurrency {
            ToolConcurrency::Serial => b"serial".to_vec(),
            ToolConcurrency::Parallel => b"parallel".to_vec(),
            ToolConcurrency::ResourceExclusive(keys) => {
                let mut encoded = b"resource-exclusive".to_vec();
                for key in keys {
                    encoded.push(0);
                    encoded.extend_from_slice(key.as_str().as_bytes());
                }
                encoded
            }
        };
        let effect_text = match effect {
            ToolEffect::ReadOnly => b"read-only".as_slice(),
            ToolEffect::Mutating => b"mutating".as_slice(),
        };
        let digest = Digest::sha256(
            DigestDomain::ToolDefinition,
            4,
            &[
                name.as_str().as_bytes(),
                semantic_version.as_bytes(),
                description.as_bytes(),
                &input_identity,
                &output_identity,
                &concurrency_text,
                effect_text,
            ],
        )?;
        Ok(Self {
            name,
            semantic_version,
            description,
            input,
            output,
            concurrency,
            effect,
            digest,
        })
    }

    /// Returns the model-facing name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the semantic version.
    #[must_use]
    pub fn semantic_version(&self) -> &str {
        &self.semantic_version
    }

    /// Returns the model-facing description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the exact input contract.
    #[must_use]
    pub const fn input(&self) -> &ToolInputContract {
        &self.input
    }

    /// Returns the exact output contract.
    #[must_use]
    pub const fn output(&self) -> &ToolOutputContract {
        &self.output
    }

    /// Returns the declared concurrency class.
    #[must_use]
    pub fn concurrency(&self) -> ToolConcurrency {
        self.concurrency.clone()
    }

    /// Returns the declared effect class.
    #[must_use]
    pub const fn effect(&self) -> ToolEffect {
        self.effect
    }

    /// Returns the canonical definition digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

/// Exact provider/profile/model/configuration target for a precompiled V2 tool view.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProjectionTargetV2 {
    integration_class: crate::IntegrationClass,
    protocol: String,
    route: String,
    integration_profile: String,
    family: String,
    requested_model: String,
    effective_model: String,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    projection_revision: u32,
}

impl ToolProjectionTargetV2 {
    /// Constructs one exact, non-fallback projection target.
    ///
    /// # Errors
    ///
    /// Returns an error when any target axis is empty, oversized, non-ASCII,
    /// model-substituted, or uses the reserved zero projection revision.
    #[allow(
        clippy::too_many_arguments,
        reason = "the release-owned projection target is one closed exact authority tuple"
    )]
    pub fn new(
        integration_class: crate::IntegrationClass,
        protocol: impl Into<String>,
        route: impl Into<String>,
        integration_profile: impl Into<String>,
        family: impl Into<String>,
        requested_model: impl Into<String>,
        effective_model: impl Into<String>,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        projection_revision: u32,
    ) -> Result<Self, RequestError> {
        let value = Self {
            integration_class,
            protocol: protocol.into(),
            route: route.into(),
            integration_profile: integration_profile.into(),
            family: family.into(),
            requested_model: requested_model.into(),
            effective_model: effective_model.into(),
            effective_provider_configuration,
            projection_revision,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), RequestError> {
        let fields = [
            self.protocol.as_str(),
            self.route.as_str(),
            self.integration_profile.as_str(),
            self.family.as_str(),
            self.requested_model.as_str(),
            self.effective_model.as_str(),
        ];
        if self.projection_revision == 0
            || self.requested_model != self.effective_model
            || fields.iter().any(|field| {
                field.is_empty()
                    || field.len() > 256
                    || !field.bytes().all(|byte| byte.is_ascii_graphic())
            })
        {
            return Err(RequestError::InvalidToolProjection {
                reason: "invalid exact target",
            });
        }
        Ok(())
    }

    /// Closed standard/custom integration class.
    #[must_use]
    pub const fn integration_class(&self) -> crate::IntegrationClass {
        self.integration_class
    }

    /// Exact protocol/dialect identity.
    #[must_use]
    pub fn protocol(&self) -> &str {
        &self.protocol
    }
    /// Exact provider route identity.
    #[must_use]
    pub fn route(&self) -> &str {
        &self.route
    }
    /// Exact integration profile identity.
    #[must_use]
    pub fn integration_profile(&self) -> &str {
        &self.integration_profile
    }
    /// Exact protocol-family identity.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }
    /// Exact requested model.
    #[must_use]
    pub fn requested_model(&self) -> &str {
        &self.requested_model
    }
    /// Exact effective model.
    #[must_use]
    pub fn effective_model(&self) -> &str {
        &self.effective_model
    }
    /// Exact effective provider configuration.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }
    /// Closed catalog revision.
    #[must_use]
    pub const fn projection_revision(&self) -> u32 {
        self.projection_revision
    }
}

/// Closed grammar syntax for a provider-visible custom-tool input format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedCustomToolGrammarSyntaxV2 {
    /// Lark grammar syntax used by Responses custom tools.
    Lark,
}

/// Exact provider-visible grammar attached to a custom-tool presentation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedCustomToolFormatV2 {
    syntax: ProjectedCustomToolGrammarSyntaxV2,
    definition: String,
}

impl ProjectedCustomToolFormatV2 {
    /// Constructs one bounded, non-empty Lark grammar definition.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or NUL-containing grammar text.
    pub fn lark(definition: impl Into<String>) -> Result<Self, RequestError> {
        let value = Self {
            syntax: ProjectedCustomToolGrammarSyntaxV2::Lark,
            definition: definition.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), RequestError> {
        if self.definition.is_empty()
            || self.definition.len() > 65_536
            || self.definition.contains('\0')
        {
            return Err(RequestError::InvalidToolProjection {
                reason: "invalid custom-tool grammar",
            });
        }
        Ok(())
    }

    /// Closed grammar syntax.
    #[must_use]
    pub const fn syntax(&self) -> ProjectedCustomToolGrammarSyntaxV2 {
        self.syntax
    }

    /// Exact grammar definition.
    #[must_use]
    pub fn definition(&self) -> &str {
        &self.definition
    }
}

/// Explicit provider-visible input presentation for one projected tool.
///
/// The discriminant, never schema shape or canonical-tool inference, selects
/// the provider wire grammar.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectedToolPresentationV2 {
    /// Responses-style structured function with a portable input schema.
    StructuredFunction {
        /// Exact provider-visible structured input schema.
        input_schema: PortableToolSchemaV2,
    },
    /// Responses-style custom tool with bounded raw UTF-8 input and grammar.
    CustomUtf8 {
        /// Exact local byte bound applied to the complete custom input.
        input: Utf8TextContractV1,
        /// Exact provider-visible grammar format.
        format: ProjectedCustomToolFormatV2,
    },
}

impl ProjectedToolPresentationV2 {
    /// Structured input schema, only for a structured-function presentation.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        match self {
            Self::StructuredFunction { input_schema } => Some(input_schema),
            Self::CustomUtf8 { .. } => None,
        }
    }

    /// Bounded UTF-8 input contract, only for a custom presentation.
    #[must_use]
    pub const fn custom_input(&self) -> Option<Utf8TextContractV1> {
        match self {
            Self::CustomUtf8 { input, .. } => Some(*input),
            Self::StructuredFunction { .. } => None,
        }
    }

    /// Exact custom format, only for a custom presentation.
    #[must_use]
    pub const fn custom_format(&self) -> Option<&ProjectedCustomToolFormatV2> {
        match self {
            Self::CustomUtf8 { format, .. } => Some(format),
            Self::StructuredFunction { .. } => None,
        }
    }
}

/// One already-validated V2 provider-visible definition over a canonical tool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedToolDefinitionV2 {
    canonical_name: ToolName,
    covered_canonical_names: Vec<ToolName>,
    exposed_name: ToolName,
    description: String,
    presentation: ProjectedToolPresentationV2,
    output_schema: PortableToolSchemaV2,
    source_identity: Digest,
}

impl ProjectedToolDefinitionV2 {
    /// Maximum UTF-8 bytes in one provider-visible projected description.
    ///
    /// Code-composition presentations embed a bounded nested tool catalog in
    /// this field, so the canonical tool-definition limit is intentionally too
    /// small. This remains a fixed global ceiling rather than an ambient
    /// provider limit.
    pub const MAX_DESCRIPTION_BYTES: usize = 65_536;

    /// Retains an exact tools-owned compiled entry without converting it through V1.
    ///
    /// # Errors
    ///
    /// Returns an error when the description or source identity is invalid.
    pub fn new(
        canonical_name: ToolName,
        mut covered_canonical_names: Vec<ToolName>,
        exposed_name: ToolName,
        description: impl Into<String>,
        input_schema: PortableToolSchemaV2,
        output_schema: PortableToolSchemaV2,
        source_identity: Digest,
    ) -> Result<Self, RequestError> {
        covered_canonical_names.sort();
        covered_canonical_names.dedup();
        let description = description.into();
        if description.is_empty()
            || description.len() > Self::MAX_DESCRIPTION_BYTES
            || covered_canonical_names.is_empty()
            || !covered_canonical_names.contains(&canonical_name)
            || source_identity.domain() != DigestDomain::ToolDefinition
            || source_identity.version() == 0
        {
            return Err(RequestError::InvalidToolProjection {
                reason: "invalid compiled definition",
            });
        }
        Ok(Self {
            canonical_name,
            covered_canonical_names,
            exposed_name,
            description,
            presentation: ProjectedToolPresentationV2::StructuredFunction { input_schema },
            output_schema,
            source_identity,
        })
    }

    /// Retains an explicit bounded UTF-8 custom-tool presentation.
    ///
    /// This constructor performs no schema inference and carries no structured
    /// input schema because the provider call grammar is raw custom input.
    ///
    /// # Errors
    ///
    /// Returns an error when the common definition axes or grammar are invalid.
    #[allow(
        clippy::too_many_arguments,
        reason = "every presentation axis is explicit"
    )]
    pub fn new_custom(
        canonical_name: ToolName,
        mut covered_canonical_names: Vec<ToolName>,
        exposed_name: ToolName,
        description: impl Into<String>,
        input: Utf8TextContractV1,
        format: ProjectedCustomToolFormatV2,
        output_schema: PortableToolSchemaV2,
        source_identity: Digest,
    ) -> Result<Self, RequestError> {
        format.validate()?;
        covered_canonical_names.sort();
        covered_canonical_names.dedup();
        let description = description.into();
        if description.is_empty()
            || description.len() > Self::MAX_DESCRIPTION_BYTES
            || covered_canonical_names.is_empty()
            || !covered_canonical_names.contains(&canonical_name)
            || source_identity.domain() != DigestDomain::ToolDefinition
            || source_identity.version() == 0
        {
            return Err(RequestError::InvalidToolProjection {
                reason: "invalid compiled definition",
            });
        }
        Ok(Self {
            canonical_name,
            covered_canonical_names,
            exposed_name,
            description,
            presentation: ProjectedToolPresentationV2::CustomUtf8 { input, format },
            output_schema,
            source_identity,
        })
    }
    /// Canonical executable tool name.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }
    /// Complete canonical operation-name set consumed by this exposure.
    #[must_use]
    pub fn covered_canonical_names(&self) -> &[ToolName] {
        &self.covered_canonical_names
    }
    /// Whether this exposure consumes one canonical operation definition.
    #[must_use]
    pub fn covers_canonical_name(&self, name: &ToolName) -> bool {
        self.covered_canonical_names.binary_search(name).is_ok()
    }
    /// Provider-visible projected name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }
    /// Provider-visible description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    /// Exact provider-visible presentation.
    #[must_use]
    pub const fn presentation(&self) -> &ProjectedToolPresentationV2 {
        &self.presentation
    }
    /// Exact V2 input schema, only for a structured-function presentation.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        self.presentation.input_schema()
    }
    /// Exact V2 output schema.
    #[must_use]
    pub const fn output_schema(&self) -> &PortableToolSchemaV2 {
        &self.output_schema
    }
    /// Tools-owned compiled entry identity.
    #[must_use]
    pub const fn source_identity(&self) -> Digest {
        self.source_identity
    }
}

/// Immutable already-compiled provider-visible V2 tool catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledToolProjectionV2 {
    target: ToolProjectionTargetV2,
    registry_identity: Digest,
    exposure_map_identity: Digest,
    definitions: Vec<ProjectedToolDefinitionV2>,
}

impl CompiledToolProjectionV2 {
    /// Freezes one exact projection catalog in caller presentation order.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty, oversized, colliding, or incorrectly
    /// identity-bound catalog.
    pub fn new(
        target: ToolProjectionTargetV2,
        registry_identity: Digest,
        exposure_map_identity: Digest,
        definitions: Vec<ProjectedToolDefinitionV2>,
    ) -> Result<Self, RequestError> {
        let value = Self {
            target,
            registry_identity,
            exposure_map_identity,
            definitions,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), RequestError> {
        self.target.validate()?;
        let mut seen_names = BTreeSet::new();
        if self.definitions.is_empty()
            || self.definitions.len() > 128
            || self
                .definitions
                .iter()
                .any(|definition| !seen_names.insert(&definition.exposed_name))
            || self.registry_identity.domain() != DigestDomain::ToolDefinition
            || self.registry_identity.version() == 0
            || self.exposure_map_identity.domain() != DigestDomain::ToolDefinition
            || self.exposure_map_identity.version() == 0
            || self.definitions.iter().any(|definition| {
                definition.description.is_empty()
                    || definition.description.len()
                        > ProjectedToolDefinitionV2::MAX_DESCRIPTION_BYTES
                    || definition.source_identity.domain() != DigestDomain::ToolDefinition
                    || definition.source_identity.version() == 0
                    || definition
                        .presentation
                        .custom_format()
                        .is_some_and(|format| format.validate().is_err())
            })
        {
            return Err(RequestError::InvalidToolProjection {
                reason: "invalid compiled catalog",
            });
        }
        Ok(())
    }
    /// Exact qualification target.
    #[must_use]
    pub const fn target(&self) -> &ToolProjectionTargetV2 {
        &self.target
    }
    /// Complete executable registry identity.
    #[must_use]
    pub const fn registry_identity(&self) -> Digest {
        self.registry_identity
    }
    /// Complete projection-map identity.
    #[must_use]
    pub const fn exposure_map_identity(&self) -> Digest {
        self.exposure_map_identity
    }
    /// Exact precompiled definitions.
    #[must_use]
    pub fn definitions(&self) -> &[ProjectedToolDefinitionV2] {
        &self.definitions
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

fn tool_input_identity(contract: &ToolInputContract) -> Vec<u8> {
    match contract {
        ToolInputContract::Structured(schema) => {
            [b"structured:".as_slice(), schema.digest().bytes()].concat()
        }
        ToolInputContract::Utf8(limit) => {
            [b"utf8:".as_slice(), &limit.max_bytes().to_be_bytes()].concat()
        }
    }
}

fn tool_output_identity(contract: &ToolOutputContract) -> Vec<u8> {
    match contract {
        ToolOutputContract::Structured(schema) => {
            [b"structured:".as_slice(), schema.digest().bytes()].concat()
        }
        ToolOutputContract::Utf8(limit) => {
            [b"utf8:".as_slice(), &limit.max_bytes().to_be_bytes()].concat()
        }
        ToolOutputContract::ModelContent(limit) => [
            b"model-content:".as_slice(),
            &limit.max_parts().to_be_bytes(),
            &limit.max_bytes().to_be_bytes(),
        ]
        .concat(),
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

#[derive(Debug, PartialEq)]
struct StableRequestPrefixInner {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    tool_projection: Option<CompiledToolProjectionV2>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
    identity: Digest,
    reuse_identity: Digest,
    canonical_bytes: Arc<[u8]>,
}

/// Immutable provider-neutral request fields shared unchanged between rounds.
#[derive(Clone, Debug, PartialEq)]
pub struct StableRequestPrefix {
    inner: Arc<StableRequestPrefixInner>,
}

#[derive(Deserialize, Serialize)]
struct StableRequestPrefixWire {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    tool_projection: Option<CompiledToolProjectionV2>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
    identity: Digest,
}

/// Separates [`StableRequestPrefix::reuse_identity`] from the full identity,
/// which hashes one part where this hashes two.
const STABLE_PREFIX_REUSE_LABEL: &[u8] = b"plak-stable-prefix-reuse-without-tool-choice";

#[derive(Serialize)]
struct StableRequestPrefixContent<'a> {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: &'a ProcessingTierDisposition,
    instructions: &'a [InstructionBlock],
    tools: &'a [ConfiguredTool],
    hosted_tools: &'a [HostedTool],
    tool_projection: Option<&'a CompiledToolProjectionV2>,
    output: &'a OutputPolicy,
    reasoning: ReasoningPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a ToolChoice>,
    cache: CachePolicy,
    limits: &'a GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: &'a NativeOptions,
    resolved_harness_plan: Option<Digest>,
}

impl StableRequestPrefix {
    /// Validates and freezes all request fields that remain stable across rounds.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate tools, unsatisfiable tool selection,
    /// structural limits, or canonical serialization failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed stable request algebra"
    )]
    pub fn new(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        processing_tier: ProcessingTierDisposition,
        instructions: Vec<InstructionBlock>,
        tools: Vec<ConfiguredTool>,
        hosted_tools: Vec<HostedTool>,
        output: OutputPolicy,
        reasoning: ReasoningPolicy,
        tool_choice: ToolChoice,
        cache: CachePolicy,
        limits: GenerationLimits,
        native: NativeOptions,
        resolved_harness_plan: Option<Digest>,
    ) -> Result<Self, RequestError> {
        Self::seal(
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier,
            instructions,
            tools,
            hosted_tools,
            None,
            output,
            reasoning,
            tool_choice,
            cache,
            limits,
            ContextAdmissionRequirement::AnyBounded,
            native,
            resolved_harness_plan,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed stable request algebra"
    )]
    fn seal(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        processing_tier: ProcessingTierDisposition,
        instructions: Vec<InstructionBlock>,
        tools: Vec<ConfiguredTool>,
        hosted_tools: Vec<HostedTool>,
        tool_projection: Option<CompiledToolProjectionV2>,
        output: OutputPolicy,
        reasoning: ReasoningPolicy,
        tool_choice: ToolChoice,
        cache: CachePolicy,
        limits: GenerationLimits,
        context_admission_requirement: ContextAdmissionRequirement,
        native: NativeOptions,
        resolved_harness_plan: Option<Digest>,
    ) -> Result<Self, RequestError> {
        validate_stable_prefix(&instructions, &tools, &hosted_tools, &tool_choice, &native)?;
        if let Some(projection) = &tool_projection {
            if projection.validate().is_err() {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection catalog invalid",
                });
            }
            if projection.target.effective_provider_configuration
                != effective_provider_configuration
            {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection effective-configuration drift",
                });
            }
            if projection.definitions.iter().any(|entry| {
                entry
                    .covered_canonical_names()
                    .iter()
                    .any(|covered| !tools.iter().any(|tool| tool.definition().name() == covered))
            }) {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection covers unbound canonical tools",
                });
            }
        }
        if resolved_harness_plan.is_some_and(|identity| {
            identity.domain() != DigestDomain::ResolvedHarnessPlan || identity.version() == 0
        }) {
            return Err(RequestError::InvalidHarnessPlanIdentity);
        }
        let mut content = StableRequestPrefixContent {
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier: &processing_tier,
            instructions: &instructions,
            tools: &tools,
            hosted_tools: &hosted_tools,
            tool_projection: tool_projection.as_ref(),
            output: &output,
            reasoning,
            tool_choice: Some(&tool_choice),
            cache,
            limits: &limits,
            context_admission_requirement,
            native: &native,
            resolved_harness_plan,
        };
        let canonical_bytes = canonical_serialize(&content)?;
        let identity = Digest::sha256(DigestDomain::StableRequestPrefix, 6, &[&canonical_bytes])?;
        content.tool_choice = None;
        let reuse_identity = Digest::sha256(
            DigestDomain::StableRequestPrefix,
            6,
            &[STABLE_PREFIX_REUSE_LABEL, &canonical_serialize(&content)?],
        )?;
        Ok(Self {
            inner: Arc::new(StableRequestPrefixInner {
                selected_provider_configuration,
                effective_provider_configuration,
                processing_tier,
                instructions,
                tools,
                hosted_tools,
                tool_projection,
                output,
                reasoning,
                tool_choice,
                cache,
                limits,
                context_admission_requirement,
                native,
                resolved_harness_plan,
                identity,
                reuse_identity,
                canonical_bytes: canonical_bytes.into(),
            }),
        })
    }

    /// Returns ordered stable instructions.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionBlock] {
        &self.inner.instructions
    }

    /// Returns the complete caller-selected provider configuration identity.
    #[must_use]
    pub fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.inner.selected_provider_configuration
    }

    /// Returns the complete compiled provider configuration identity.
    #[must_use]
    pub fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.inner.effective_provider_configuration
    }

    /// Returns the exact provider processing/service-tier selection.
    #[must_use]
    pub fn processing_tier(&self) -> &ProcessingTierDisposition {
        &self.inner.processing_tier
    }

    /// Returns ordered configured function tools.
    #[must_use]
    pub fn tools(&self) -> &[ConfiguredTool] {
        &self.inner.tools
    }

    /// Returns exact identity-bearing provider-hosted tool selections.
    #[must_use]
    pub fn hosted_tools(&self) -> &[HostedTool] {
        &self.inner.hosted_tools
    }

    /// Returns an exact precompiled V2 view when the upstream registry supplied one.
    #[must_use]
    pub fn tool_projection(&self) -> Option<&CompiledToolProjectionV2> {
        self.inner.tool_projection.as_ref()
    }

    /// Binds an upstream-compiled V2 view into this immutable prefix.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection does not bind this prefix's exact
    /// effective provider configuration and canonical tools.
    pub fn with_tool_projection(
        &self,
        projection: CompiledToolProjectionV2,
    ) -> Result<Self, RequestError> {
        Self::seal(
            self.selected_provider_configuration_id(),
            self.effective_provider_configuration_id(),
            self.processing_tier().clone(),
            self.instructions().to_vec(),
            self.tools().to_vec(),
            self.hosted_tools().to_vec(),
            Some(projection),
            self.output().clone(),
            self.reasoning(),
            self.tool_choice().clone(),
            self.cache(),
            self.limits().clone(),
            self.context_admission_requirement(),
            self.native().clone(),
            self.resolved_harness_plan_identity(),
        )
    }

    /// Returns stable output intent.
    #[must_use]
    pub fn output(&self) -> &OutputPolicy {
        &self.inner.output
    }

    /// Returns stable reasoning intent.
    #[must_use]
    pub fn reasoning(&self) -> ReasoningPolicy {
        self.inner.reasoning
    }

    /// Returns stable tool-selection intent.
    #[must_use]
    pub fn tool_choice(&self) -> &ToolChoice {
        &self.inner.tool_choice
    }

    /// Returns stable cache intent.
    #[must_use]
    pub fn cache(&self) -> CachePolicy {
        self.inner.cache
    }

    /// Returns stable portable generation limits.
    #[must_use]
    pub fn limits(&self) -> &GenerationLimits {
        &self.inner.limits
    }

    /// Returns the `evidence_class` policy required before provider-visible work.
    #[must_use]
    pub fn context_admission_requirement(&self) -> ContextAdmissionRequirement {
        self.inner.context_admission_requirement
    }

    /// Rebinds this prefix to an exact context-evidence policy and fresh identity.
    ///
    /// # Errors
    ///
    /// Returns an error only if canonical identity construction fails.
    pub fn with_context_admission_requirement(
        &self,
        requirement: ContextAdmissionRequirement,
    ) -> Result<Self, RequestError> {
        Self::seal(
            self.selected_provider_configuration_id(),
            self.effective_provider_configuration_id(),
            self.processing_tier().clone(),
            self.instructions().to_vec(),
            self.tools().to_vec(),
            self.hosted_tools().to_vec(),
            self.tool_projection().cloned(),
            self.output().clone(),
            self.reasoning(),
            self.tool_choice().clone(),
            self.cache(),
            self.limits().clone(),
            requirement,
            self.native().clone(),
            self.resolved_harness_plan_identity(),
        )
    }

    /// Returns stable advanced namespaced options.
    #[must_use]
    pub fn native(&self) -> &NativeOptions {
        &self.inner.native
    }

    /// Returns the core-resolved harness-plan identity bound at construction.
    #[must_use]
    pub fn resolved_harness_plan_identity(&self) -> Option<Digest> {
        self.inner.resolved_harness_plan
    }

    /// Returns the complete versioned prefix identity.
    #[must_use]
    pub fn identity(&self) -> Digest {
        self.inner.identity
    }

    /// Returns the identity of this prefix with tool selection left out.
    ///
    /// Native continuation state binds this identity, not [`Self::identity`]:
    /// a round that only changes `tool_choice` (the operation-budget wrap-up)
    /// still continues the state the previous round produced, while every
    /// other field difference makes the state a different prefix's.
    #[must_use]
    pub fn reuse_identity(&self) -> Digest {
        self.inner.reuse_identity
    }

    /// Returns exact deterministic bytes for every stable request field.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.inner.canonical_bytes
    }

    /// Returns a conservative retained-memory bound for this shared prefix.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.inner
            .canonical_bytes
            .len()
            .saturating_mul(2)
            .saturating_add(size_of::<StableRequestPrefixInner>())
    }

    /// Reports whether two handles share the exact immutable prefix allocation.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Names the fields whose values differ from `other`, without content.
    #[must_use]
    pub fn drift_from(&self, other: &Self) -> StableRequestPrefixDrift {
        use StableRequestPrefixField as Field;
        let (a, b) = (&*self.inner, &*other.inner);
        [
            (
                a.selected_provider_configuration != b.selected_provider_configuration,
                Field::SelectedProviderConfiguration,
            ),
            (
                a.effective_provider_configuration != b.effective_provider_configuration,
                Field::EffectiveProviderConfiguration,
            ),
            (
                a.processing_tier != b.processing_tier,
                Field::ProcessingTier,
            ),
            (a.instructions != b.instructions, Field::Instructions),
            (a.tools != b.tools, Field::Tools),
            (a.hosted_tools != b.hosted_tools, Field::HostedTools),
            (
                a.tool_projection != b.tool_projection,
                Field::ToolProjection,
            ),
            (a.output != b.output, Field::Output),
            (a.reasoning != b.reasoning, Field::Reasoning),
            (a.tool_choice != b.tool_choice, Field::ToolChoice),
            (a.cache != b.cache, Field::Cache),
            (a.limits != b.limits, Field::Limits),
            (
                a.context_admission_requirement != b.context_admission_requirement,
                Field::ContextAdmissionRequirement,
            ),
            (a.native != b.native, Field::Native),
            (
                a.resolved_harness_plan != b.resolved_harness_plan,
                Field::ResolvedHarnessPlan,
            ),
        ]
        .into_iter()
        .filter(|(differs, _)| *differs)
        .fold(StableRequestPrefixDrift::default(), |drift, (_, field)| {
            drift.with(field)
        })
    }
}

/// One field of a [`StableRequestPrefix`], named for content-free diagnostics.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StableRequestPrefixField {
    /// The caller-selected provider configuration identity.
    SelectedProviderConfiguration,
    /// The compiled provider configuration identity.
    EffectiveProviderConfiguration,
    /// The provider processing/service-tier selection.
    ProcessingTier,
    /// Ordered stable instructions.
    Instructions,
    /// Locally executed tool definitions.
    Tools,
    /// Provider-hosted tools.
    HostedTools,
    /// The compiled provider-visible tool projection.
    ToolProjection,
    /// Output intent.
    Output,
    /// Reasoning intent.
    Reasoning,
    /// Tool-selection intent.
    ToolChoice,
    /// Cache intent.
    Cache,
    /// Generation limits.
    Limits,
    /// The required context-evidence class.
    ContextAdmissionRequirement,
    /// Provider-native options.
    Native,
    /// The resolved harness-plan identity.
    ResolvedHarnessPlan,
}

impl StableRequestPrefixField {
    /// Every field, in declaration order.
    pub const ALL: [Self; 15] = [
        Self::SelectedProviderConfiguration,
        Self::EffectiveProviderConfiguration,
        Self::ProcessingTier,
        Self::Instructions,
        Self::Tools,
        Self::HostedTools,
        Self::ToolProjection,
        Self::Output,
        Self::Reasoning,
        Self::ToolChoice,
        Self::Cache,
        Self::Limits,
        Self::ContextAdmissionRequirement,
        Self::Native,
        Self::ResolvedHarnessPlan,
    ];

    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelectedProviderConfiguration => "selected-provider-configuration",
            Self::EffectiveProviderConfiguration => "effective-provider-configuration",
            Self::ProcessingTier => "processing-tier",
            Self::Instructions => "instructions",
            Self::Tools => "tools",
            Self::HostedTools => "hosted-tools",
            Self::ToolProjection => "tool-projection",
            Self::Output => "output",
            Self::Reasoning => "reasoning",
            Self::ToolChoice => "tool-choice",
            Self::Cache => "cache",
            Self::Limits => "limits",
            Self::ContextAdmissionRequirement => "context-admission-requirement",
            Self::Native => "native",
            Self::ResolvedHarnessPlan => "harness-plan",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// The set of [`StableRequestPrefixField`]s that differ between two prefixes.
///
/// `Copy` and content-free, so it can travel in telemetry and error text.
/// Displays as the comma-separated field names in declaration order.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StableRequestPrefixDrift {
    fields: u16,
}

impl StableRequestPrefixDrift {
    /// Returns this set with `field` added.
    #[must_use]
    pub const fn with(self, field: StableRequestPrefixField) -> Self {
        Self {
            fields: self.fields | field.bit(),
        }
    }

    /// Reports whether no field differs.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.fields == 0
    }

    /// Reports whether `field` differs.
    #[must_use]
    pub const fn contains(self, field: StableRequestPrefixField) -> bool {
        self.fields & field.bit() != 0
    }

    /// Iterates the differing fields in declaration order.
    pub fn fields(self) -> impl Iterator<Item = StableRequestPrefixField> {
        StableRequestPrefixField::ALL
            .into_iter()
            .filter(move |field| self.contains(*field))
    }
}

impl fmt::Display for StableRequestPrefixDrift {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return formatter.write_str("none");
        }
        for (index, field) in self.fields().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(field.as_str())?;
        }
        Ok(())
    }
}

impl Serialize for StableRequestPrefix {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        StableRequestPrefixWire {
            selected_provider_configuration: self.selected_provider_configuration_id(),
            effective_provider_configuration: self.effective_provider_configuration_id(),
            processing_tier: self.processing_tier().clone(),
            instructions: self.instructions().to_vec(),
            tools: self.tools().to_vec(),
            hosted_tools: self.hosted_tools().to_vec(),
            tool_projection: self.tool_projection().cloned(),
            output: self.output().clone(),
            reasoning: self.reasoning(),
            tool_choice: self.tool_choice().clone(),
            cache: self.cache(),
            limits: self.limits().clone(),
            context_admission_requirement: self.context_admission_requirement(),
            native: self.native().clone(),
            resolved_harness_plan: self.resolved_harness_plan_identity(),
            identity: self.identity(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StableRequestPrefix {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = StableRequestPrefixWire::deserialize(deserializer)?;
        let prefix = Self::seal(
            wire.selected_provider_configuration,
            wire.effective_provider_configuration,
            wire.processing_tier,
            wire.instructions,
            wire.tools,
            wire.hosted_tools,
            wire.tool_projection,
            wire.output,
            wire.reasoning,
            wire.tool_choice,
            wire.cache,
            wire.limits,
            wire.context_admission_requirement,
            wire.native,
            wire.resolved_harness_plan,
        )
        .map_err(serde::de::Error::custom)?;
        if prefix.identity() != wire.identity {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "stable request prefix",
            }));
        }
        Ok(prefix)
    }
}

/// Who initiated a root turn, independent of subsequent steering or tool rounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnOrigin {
    /// A user submitted a prompt.
    UserPrompt,
    /// The host woke the agent (child results, or host job notices).
    HostWake,
}

impl TurnOrigin {
    /// Content-free telemetry spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserPrompt => "user_prompt",
            Self::HostWake => "host_wake",
        }
    }
}

/// A private-fielded, validated, provider-neutral model request.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    prefix: StableRequestPrefix,
    input: ConversationView,
    metadata: RequestMetadata,
    model_input_identity: Digest,
    identity: Digest,
    canonical_bytes: Arc<[u8]>,
}

#[derive(Serialize)]
struct ModelRequestWireRef<'a> {
    prefix: &'a StableRequestPrefix,
    input: &'a ConversationView,
    metadata: RequestMetadata,
    identity: Digest,
}

#[derive(Serialize)]
struct ModelRequestIdentityContent {
    prefix: Digest,
    input: Digest,
    metadata: RequestMetadata,
}

#[derive(Serialize)]
struct ModelInputIdentityContent {
    prefix: Digest,
    input: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRequestWire {
    prefix: StableRequestPrefix,
    input: ConversationView,
    metadata: RequestMetadata,
    identity: Digest,
}

impl<'de> Deserialize<'de> for ModelRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelRequestWire::deserialize(deserializer)?;
        let request = Self::freeze(wire.prefix, wire.input, wire.metadata)
            .map_err(serde::de::Error::custom)?;
        if request.identity() != wire.identity {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "model request",
            }));
        }
        Ok(request)
    }
}

impl Serialize for ModelRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ModelRequestWireRef {
            prefix: &self.prefix,
            input: &self.input,
            metadata: self.metadata,
            identity: self.identity,
        }
        .serialize(serializer)
    }
}

impl ModelRequest {
    /// Derives the root origin from its most recent intent, not later steering.
    /// Returns `None` for projections without a retained prompt identity.
    #[must_use]
    pub fn turn_origin(&self) -> Option<TurnOrigin> {
        Some(
            if self.turn_prompt()?.provenance() == crate::Provenance::UserInput {
                TurnOrigin::UserPrompt
            } else {
                TurnOrigin::HostWake
            },
        )
    }

    /// The item that opened the current root turn: the prompt or host-wake
    /// instruction in the most recent intent segment, never later steering or
    /// a note appended inside the turn. A projection without an intent
    /// segment falls back to its last user text.
    #[must_use]
    pub fn turn_prompt(&self) -> Option<&crate::ConversationItem> {
        let prompt = |item: &&crate::ConversationItem| {
            item.actor() == crate::Actor::User
                && matches!(
                    item.provenance(),
                    crate::Provenance::UserInput | crate::Provenance::Instruction
                )
                && item
                    .blocks()
                    .iter()
                    .any(|block| matches!(block, crate::ContentBlock::Text(_)))
        };
        let intent = self
            .input
            .history()
            .segments()
            .iter()
            .chain(self.input.working().segments())
            .filter(|segment| segment.kind() == crate::HistorySegmentKind::RecentIntent)
            .last();
        if let Some(intent) = intent {
            intent.items().iter().rev().find(prompt)
        } else {
            self.input.items().into_iter().filter(prompt).last()
        }
    }

    /// Starts a validated request builder with owned history and metadata.
    #[must_use]
    pub fn builder(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> ModelRequestBuilder {
        ModelRequestBuilder {
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier: ProcessingTierDisposition::Standard,
            instructions: Vec::new(),
            input,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            output: OutputPolicy::default(),
            reasoning: ReasoningPolicy::default(),
            tool_choice: ToolChoice::default(),
            cache: CachePolicy::default(),
            limits: GenerationLimits::default(),
            context_admission_requirement: ContextAdmissionRequirement::AnyBounded,
            metadata,
            native: NativeOptions::default(),
            resolved_harness_plan: None,
        }
    }

    /// Constructs a request from an already validated shared prefix and a new projection.
    ///
    /// This is the normal round boundary when canonical history advances: it
    /// preserves the immutable prefix allocation while validating the complete
    /// replacement projection and deriving fresh request identity bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection contains an incomplete protected
    /// group or deterministic identity construction fails.
    pub fn from_parts(
        prefix: StableRequestPrefix,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> Result<Self, RequestError> {
        Self::freeze(prefix, input, metadata)
    }

    fn freeze(
        prefix: StableRequestPrefix,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> Result<Self, RequestError> {
        input.validate_model_visible()?;
        let model_input_bytes = canonical_serialize(&ModelInputIdentityContent {
            prefix: prefix.identity(),
            input: input.identity(),
        })?;
        let model_input_identity =
            Digest::sha256(DigestDomain::RequestPlan, 7, &[&model_input_bytes])?;
        let canonical_bytes = canonical_serialize(&ModelRequestIdentityContent {
            prefix: prefix.identity(),
            input: input.identity(),
            metadata,
        })?;
        let identity = Digest::sha256(DigestDomain::RequestPlan, 6, &[&canonical_bytes])?;
        Ok(Self {
            prefix,
            input,
            metadata,
            model_input_identity,
            identity,
            canonical_bytes: canonical_bytes.into(),
        })
    }

    /// Returns the immutable stable prefix shared by this request.
    #[must_use]
    pub const fn stable_prefix(&self) -> &StableRequestPrefix {
        &self.prefix
    }

    /// Constructs a new round while sharing the complete stable prefix and history projection.
    ///
    /// # Errors
    ///
    /// Returns an error only if deterministic request identity construction fails.
    pub fn for_round(&self, metadata: RequestMetadata) -> Result<Self, RequestError> {
        Self::from_parts(self.prefix.clone(), self.input.clone(), metadata)
    }

    /// Returns the complete deterministic request identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the exact identity of every model-visible input fact.
    ///
    /// Unlike [`Self::identity`], this excludes attempt metadata that is not
    /// sent to the model.
    #[must_use]
    pub const fn model_input_identity(&self) -> Digest {
        self.model_input_identity
    }

    /// Returns deterministic identity bytes for all request-affecting artifacts.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns a conservative retained-memory bound without flattening history.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.prefix
            .retained_bytes_upper_bound()
            .saturating_add(self.input.retained_bytes_upper_bound())
            .saturating_add(self.canonical_bytes.len())
    }

    /// Returns ordered instructions.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionBlock] {
        self.prefix.instructions()
    }

    /// Returns the immutable conversation snapshot.
    #[must_use]
    pub const fn input(&self) -> &ConversationView {
        &self.input
    }

    /// Returns ordered configured function tools.
    #[must_use]
    pub fn tools(&self) -> &[ConfiguredTool] {
        self.prefix.tools()
    }

    /// Returns exact identity-bearing provider-hosted tool selections.
    #[must_use]
    pub fn hosted_tools(&self) -> &[HostedTool] {
        self.prefix.hosted_tools()
    }

    /// Returns the exact upstream-compiled V2 provider-visible tool view.
    #[must_use]
    pub fn tool_projection(&self) -> Option<&CompiledToolProjectionV2> {
        self.prefix.tool_projection()
    }

    /// The runtime owner a protocol decodes a call to the provider-visible
    /// `exposed` tool with: the MCP server its configured tool names, or the
    /// local runtime for every other locally dispatched name.
    #[must_use]
    pub fn local_tool_owner(&self, exposed: &ToolName) -> ToolExecutionOwner {
        let canonical = self
            .tool_projection()
            .and_then(|projection| {
                projection
                    .definitions()
                    .iter()
                    .find(|entry| entry.exposed_name() == exposed)
            })
            .map_or(exposed, ProjectedToolDefinitionV2::canonical_name);
        self.tools()
            .iter()
            .find(|tool| tool.definition().name() == canonical)
            .and_then(ConfiguredTool::local_owner)
            .unwrap_or(ToolExecutionOwner::Local)
    }

    /// Returns output intent.
    #[must_use]
    pub fn output(&self) -> &OutputPolicy {
        &self.prefix.inner.output
    }

    /// Returns reasoning intent.
    #[must_use]
    pub fn reasoning(&self) -> ReasoningPolicy {
        self.prefix.inner.reasoning
    }

    /// Returns tool-selection intent.
    #[must_use]
    pub fn tool_choice(&self) -> &ToolChoice {
        &self.prefix.inner.tool_choice
    }

    /// Returns cache intent.
    #[must_use]
    pub fn cache(&self) -> CachePolicy {
        self.prefix.inner.cache
    }

    /// Returns portable generation limits.
    #[must_use]
    pub fn limits(&self) -> &GenerationLimits {
        &self.prefix.inner.limits
    }

    /// Returns stable request metadata.
    #[must_use]
    pub const fn metadata(&self) -> RequestMetadata {
        self.metadata
    }

    /// Returns advanced namespaced options.
    #[must_use]
    pub fn native(&self) -> &NativeOptions {
        &self.prefix.inner.native
    }

    /// Returns the core-resolved harness-plan identity carried by this request.
    #[must_use]
    pub fn resolved_harness_plan_identity(&self) -> Option<Digest> {
        self.prefix.resolved_harness_plan_identity()
    }

    /// Returns the complete caller-selected provider configuration identity.
    #[must_use]
    pub fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.prefix.selected_provider_configuration_id()
    }

    /// Returns the complete compiled provider configuration identity.
    #[must_use]
    pub fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.prefix.effective_provider_configuration_id()
    }

    /// Returns the exact provider processing/service-tier selection.
    #[must_use]
    pub fn processing_tier(&self) -> &ProcessingTierDisposition {
        self.prefix.processing_tier()
    }

    /// Returns the context-evidence policy bound into this request identity.
    #[must_use]
    pub fn context_admission_requirement(&self) -> ContextAdmissionRequirement {
        self.prefix.context_admission_requirement()
    }
}

/// Builder for a validated [`ModelRequest`].
#[derive(Debug)]
pub struct ModelRequestBuilder {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    input: ConversationView,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    metadata: RequestMetadata,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
}

impl ModelRequestBuilder {
    /// Sets the exact provider processing/service tier.
    #[must_use]
    pub fn processing_tier(mut self, processing_tier: ProcessingTierDisposition) -> Self {
        self.processing_tier = processing_tier;
        self
    }

    /// Sets ordered instructions.
    #[must_use]
    pub fn instructions(mut self, instructions: Vec<InstructionBlock>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Sets ordered configured function tools.
    #[must_use]
    pub fn tools(mut self, tools: Vec<ConfiguredTool>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets exact identity-bearing provider-hosted tools.
    #[must_use]
    pub fn hosted_tools(mut self, hosted_tools: Vec<HostedTool>) -> Self {
        self.hosted_tools = hosted_tools;
        self
    }

    /// Sets output intent.
    #[must_use]
    pub fn output(mut self, output: OutputPolicy) -> Self {
        self.output = output;
        self
    }

    /// Sets reasoning intent.
    #[must_use]
    pub fn reasoning(mut self, reasoning: ReasoningPolicy) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Sets tool-selection intent.
    #[must_use]
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = tool_choice;
        self
    }

    /// Sets cache intent.
    #[must_use]
    pub fn cache(mut self, cache: CachePolicy) -> Self {
        self.cache = cache;
        self
    }

    /// Sets portable generation limits.
    #[must_use]
    pub fn limits(mut self, limits: GenerationLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Requires an exact context-evidence class before provider-visible work.
    #[must_use]
    pub const fn context_admission_requirement(
        mut self,
        requirement: ContextAdmissionRequirement,
    ) -> Self {
        self.context_admission_requirement = requirement;
        self
    }

    /// Sets advanced namespaced options.
    #[must_use]
    pub fn native(mut self, native: NativeOptions) -> Self {
        self.native = native;
        self
    }

    /// Binds the exact core-resolved harness plan that produced this request.
    #[must_use]
    pub fn resolved_harness_plan_identity(mut self, identity: Digest) -> Self {
        self.resolved_harness_plan = Some(identity);
        self
    }

    /// Validates and freezes the request.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate tool names or an unsatisfiable tool choice.
    pub fn build(self) -> Result<ModelRequest, RequestError> {
        let prefix = StableRequestPrefix::new(
            self.selected_provider_configuration,
            self.effective_provider_configuration,
            self.processing_tier,
            self.instructions,
            self.tools,
            self.hosted_tools,
            self.output,
            self.reasoning,
            self.tool_choice,
            self.cache,
            self.limits,
            self.native,
            self.resolved_harness_plan,
        )?
        .with_context_admission_requirement(self.context_admission_requirement)?;
        ModelRequest::freeze(prefix, self.input, self.metadata)
    }
}

fn validate_stable_prefix(
    instructions: &[InstructionBlock],
    tools: &[ConfiguredTool],
    hosted_tools: &[HostedTool],
    tool_choice: &ToolChoice,
    native: &NativeOptions,
) -> Result<(), RequestError> {
    if instructions.len() > 256 {
        return Err(RequestError::TooManyInstructions);
    }
    if tools.len() > 128 {
        return Err(RequestError::TooManyTools);
    }
    if native.entries.len() > 64 {
        return Err(RequestError::TooManyNativeOptions);
    }
    for value in native.entries.values() {
        if canonicalize_normalized_prevalidated(value.clone(), usize::MAX)?.len() > 1024 * 1024 {
            return Err(RequestError::TooLong {
                field: "native option",
                maximum: 1024 * 1024,
            });
        }
    }
    let mut names = BTreeSet::new();
    for tool in tools {
        if !names.insert(tool.definition().name().clone()) {
            return Err(RequestError::DuplicateTool(
                tool.definition().name().clone(),
            ));
        }
    }
    let mut hosted_tools = hosted_tools.to_vec();
    hosted_tools.sort_unstable();
    if hosted_tools.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(RequestError::InvalidHostedTools);
    }
    let programmatic_selected = hosted_tools.contains(&HostedTool::ProgrammaticComposition);
    let has_programmatic_caller = tools
        .iter()
        .any(|tool| !matches!(tool.caller(), ToolCallerDisposition::DirectOnly));
    if programmatic_selected != has_programmatic_caller {
        return Err(RequestError::InvalidHostedTools);
    }
    match tool_choice {
        ToolChoice::Required if tools.is_empty() => Err(RequestError::RequiredToolWithoutTools),
        ToolChoice::Specific(name) if !names.contains(name) => {
            Err(RequestError::UnknownTool(name.clone()))
        }
        _ => Ok(()),
    }
}

/// Canonical bytes of an identity document. The document is bounded by its
/// owner (tool count, description and schema caps, instruction parts), not by
/// the tool-schema limits: a wildcard MCP catalog alone exceeds their node
/// count. Content valid under [`crate::SemanticJsonV1`] keeps its bytes.
fn canonical_serialize(value: &impl Serialize) -> Result<Vec<u8>, RequestError> {
    let value = serde_json::to_value(value).map_err(|error| RequestError::CanonicalJson {
        reason: error.to_string(),
    })?;
    Ok(canonicalize_normalized_prevalidated(
        value,
        MAX_IDENTITY_DOCUMENT_BYTES,
    )?)
}

/// Memory guard for one canonical identity document; its owner's own caps
/// bind it far below this.
const MAX_IDENTITY_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

fn ensure_nonempty(value: &str, field: &'static str) -> Result<(), RequestError> {
    if value.is_empty() {
        Err(RequestError::Empty { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
