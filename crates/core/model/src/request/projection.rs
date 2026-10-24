//! The compiled V2 tool projection presented to a provider.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, PortableToolSchemaV2, ToolName,
};

use super::{RequestError, Utf8TextContractV1};

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

    pub(super) fn validate(&self) -> Result<(), RequestError> {
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
