//! Explicit provider configuration and its deterministic selected-to-effective binding.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    CachePolicy, Capability, ContextAdmissionRequirement, CredentialSelectionId, Digest,
    DigestDomain, LanguageModel, McpServerId, ModelAdapterProfileId, ModelFamilyId, ModelId,
    NamespaceId, OutputFormat, OutputPolicy, ProtocolId, ProviderAuthorityClassId,
    ProviderProfileRevisionId, ProviderServiceTierId, ReasoningEffort, ReasoningPolicy,
    ResolvedModelDescriptor, RouteId, RuntimeModelAuthority, StandardIntegrationAuthority, Support,
    Timestamp, ToolChoice, ToolDefinition, ToolExecutionOwner,
};

/// Selected-configuration schema revision.
pub const SELECTED_PROVIDER_CONFIGURATION_REVISION: u32 = 5;
/// Effective-configuration compiler revision.
pub const EFFECTIVE_PROVIDER_CONFIGURATION_COMPILER_REVISION: u32 = 6;
/// Maximum canonical family-policy payload size.
pub const MAX_FAMILY_POLICY_BYTES: usize = 64 * 1024;
/// Maximum bounded provider-owned semantic evidence payload size.
pub const MAX_PROVIDER_SEMANTIC_EVIDENCE_BYTES: usize = 64 * 1024;
/// Output ceiling for driver-owned semantic compaction with tool selection
/// disabled, when the route's turn bound and reasoning leave room for it.
pub const SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS: NonZeroU32 = match NonZeroU32::new(16_384) {
    Some(value) => value,
    None => unreachable!(),
};
/// Output reserved for the summary text itself inside the compaction bound.
pub const SEMANTIC_COMPACTION_SUMMARY_TOKENS: u32 = 8_192;

/// Output bound for driver-owned semantic compaction on a route whose turns
/// carry `configured` and reason under `reasoning`.
///
/// The summary gets [`SEMANTIC_COMPACTION_SUMMARY_TOKENS`] above a thinking
/// reserve. An explicit thinking budget is that reserve, because Messages
/// requires the budget to stay below the bound; any other policy shares
/// [`SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS`]. The bound never exceeds a
/// configured turn bound, so compaction reserves no more output than a turn
/// does and the request stays an exact function of the configuration.
#[must_use]
pub fn semantic_compaction_output_bound(
    configured: Option<NonZeroU32>,
    reasoning: ReasoningPolicy,
) -> NonZeroU32 {
    let wanted = match reasoning {
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::TokenBudget(budget)),
            ..
        } => budget.saturating_add(SEMANTIC_COMPACTION_SUMMARY_TOKENS),
        _ => SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS,
    };
    configured.map_or(wanted, |configured| configured.min(wanted))
}
/// Trailing user instruction for harness-side compact.
pub const SEMANTIC_COMPACTION_INSTRUCTION: &str =
    crate::prompts::PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION;

/// Immutable executable implementation identity retained across reconstruction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelImplementationIdentity {
    integration_class: crate::IntegrationClass,
    implementation: NamespaceId,
    revision: u32,
    provenance: NamespaceId,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelImplementationIdentityWire {
    integration_class: crate::IntegrationClass,
    implementation: NamespaceId,
    revision: u32,
    provenance: NamespaceId,
    identity: Digest,
}

impl<'de> Deserialize<'de> for ModelImplementationIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelImplementationIdentityWire::deserialize(deserializer)?;
        let value = Self::from_parts(
            wire.integration_class,
            wire.implementation,
            wire.revision,
            wire.provenance,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(value)
    }
}

impl ModelImplementationIdentity {
    /// Creates one bounded implementation/revision/provenance identity.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero prototype revision or digest failure.
    pub fn custom(
        implementation: NamespaceId,
        revision: u32,
        provenance: NamespaceId,
    ) -> Result<Self, ProviderConfigurationError> {
        Self::from_parts(
            crate::IntegrationClass::Custom,
            implementation,
            revision,
            provenance,
        )
    }

    fn from_parts(
        integration_class: crate::IntegrationClass,
        implementation: NamespaceId,
        revision: u32,
        provenance: NamespaceId,
    ) -> Result<Self, ProviderConfigurationError> {
        if revision == 0 {
            return Err(ProviderConfigurationError::UnsupportedRevision);
        }
        if (integration_class == crate::IntegrationClass::Standard)
            != (provenance.as_str() == "plakous.release")
        {
            return Err(ProviderConfigurationError::ForeignIntegrationClass);
        }
        let bytes =
            serde_json::to_vec(&(integration_class, &implementation, revision, &provenance))
                .map_err(|_| ProviderConfigurationError::Identity)?;
        let identity = Digest::sha256(
            DigestDomain::ModelBinding,
            7,
            &[b"plakous-model/implementation-identity-v2", &bytes],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(Self {
            integration_class,
            implementation,
            revision,
            provenance,
            identity,
        })
    }

    /// Derives the maintained implementation identity for one release-owned profile.
    ///
    /// This constructor is implementation plumbing and must not be reexported by
    /// the curated custom-registration facade.
    #[doc(hidden)]
    pub fn release_owned(
        _authority: &StandardIntegrationAuthority,
        descriptor: &ResolvedModelDescriptor,
        revision: u32,
    ) -> Result<Self, ProviderConfigurationError> {
        let implementation = NamespaceId::new(descriptor.profile().as_str())
            .map_err(|_| ProviderConfigurationError::Identity)?;
        let provenance = NamespaceId::new("plakous.release")
            .map_err(|_| ProviderConfigurationError::Identity)?;
        Self::from_parts(
            crate::IntegrationClass::Standard,
            implementation,
            revision,
            provenance,
        )
    }

    /// Returns the closed owner of this executable implementation.
    #[must_use]
    pub const fn integration_class(&self) -> crate::IntegrationClass {
        self.integration_class
    }

    /// Returns the stable implementation name.
    #[must_use]
    pub const fn implementation(&self) -> &NamespaceId {
        &self.implementation
    }
    /// Returns the behavior-bearing implementation revision.
    #[must_use]
    pub const fn revision(&self) -> u32 {
        self.revision
    }
    /// Returns the immutable implementation provenance label.
    #[must_use]
    pub const fn provenance(&self) -> &NamespaceId {
        &self.provenance
    }
    /// Returns the complete implementation identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

macro_rules! provider_configuration_id {
    ($name:ident, $domain:expr, $version:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(Digest);

        impl $name {
            pub(crate) fn new(digest: Digest) -> Result<Self, ProviderConfigurationError> {
                if digest.domain() != $domain || digest.version() != $version {
                    return Err(ProviderConfigurationError::Integrity);
                }
                Ok(Self(digest))
            }

            /// Returns the exact domain-separated digest.
            #[must_use]
            pub const fn digest(self) -> Digest {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let digest = Digest::deserialize(deserializer)?;
                Self::new(digest).map_err(serde::de::Error::custom)
            }
        }
    };
}

provider_configuration_id!(
    SelectedProviderConfigurationId,
    DigestDomain::SelectedProviderConfiguration,
    5,
    "Typed identity of one complete caller-selected provider configuration."
);

/// Fresh provider authority sealed to one exact effective configuration.
///
/// Provider adapters own credential and catalog revalidation, while this
/// aggregate closes that fresh evidence over the provider-neutral compiled
/// behavior. It is the attempt-authority value consumed by runtime admission
/// and final dispatch; raw [`RuntimeModelAuthority`] alone never grants either.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfiguredRuntimeAuthority {
    effective_configuration: EffectiveProviderConfigurationId,
    runtime: RuntimeModelAuthority,
    binding: Digest,
}

impl std::fmt::Debug for ConfiguredRuntimeAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredRuntimeAuthority")
            .field("effective_configuration", &self.effective_configuration)
            .field("binding", &self.binding)
            .field("authority", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ConfiguredRuntimeAuthority {
    /// Returns the exact effective configuration authorized for this attempt.
    #[must_use]
    pub const fn effective_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_configuration
    }

    /// Returns the fresh provider-owned evidence sealed by this value.
    #[must_use]
    pub const fn runtime(&self) -> &RuntimeModelAuthority {
        &self.runtime
    }

    /// Returns the complete credential-free attempt-authority binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }
}
provider_configuration_id!(
    EffectiveProviderConfigurationId,
    DigestDomain::EffectiveProviderConfiguration,
    5,
    "Typed identity of one complete compiled semantic provider configuration."
);

/// Bounded provider-owned semantic evidence with refresh-only fields removed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderSemanticEvidence {
    schema_revision: u32,
    canonical_bytes: Vec<u8>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSemanticEvidenceWire {
    schema_revision: u32,
    canonical_bytes: Vec<u8>,
    identity: Digest,
}

impl<'de> Deserialize<'de> for ProviderSemanticEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProviderSemanticEvidenceWire::deserialize(deserializer)?;
        let value = Self::new(wire.schema_revision, wire.canonical_bytes)
            .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(value)
    }
}

impl ProviderSemanticEvidence {
    /// Seals one provider-owned canonical semantic projection.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero revision, empty/oversized bytes, or digest failure.
    pub fn new(
        schema_revision: u32,
        canonical_bytes: Vec<u8>,
    ) -> Result<Self, ProviderConfigurationError> {
        if schema_revision == 0
            || canonical_bytes.is_empty()
            || canonical_bytes.len() > MAX_PROVIDER_SEMANTIC_EVIDENCE_BYTES
        {
            return Err(ProviderConfigurationError::InvalidSemanticEvidence);
        }
        let identity = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"plakous/provider-semantic-evidence-v1",
                &schema_revision.to_be_bytes(),
                &canonical_bytes,
            ],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(Self {
            schema_revision,
            canonical_bytes,
            identity,
        })
    }

    /// Returns the provider evidence schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }
    /// Returns the reconstruction-complete canonical semantic facts.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
    /// Returns the evidence identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Stable resolved provider authority used by semantic configuration identity.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedProviderAuthorityBinding {
    semantic_descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    semantic_evidence: ProviderSemanticEvidence,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedProviderAuthorityBindingWire {
    semantic_descriptor_binding: Digest,
    account_binding: Digest,
    credential_authority_binding: Digest,
    semantic_evidence: ProviderSemanticEvidence,
    identity: Digest,
}

impl<'de> Deserialize<'de> for ResolvedProviderAuthorityBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResolvedProviderAuthorityBindingWire::deserialize(deserializer)?;
        let value = Self::from_bindings(
            wire.semantic_descriptor_binding,
            wire.account_binding,
            wire.credential_authority_binding,
            wire.semantic_evidence,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(value)
    }
}

impl ResolvedProviderAuthorityBinding {
    /// Builds stable semantic authority from one fresh execution witness.
    ///
    /// # Errors
    ///
    /// Returns an error when descriptor, account, credential authority, or
    /// evidence bindings are foreign or cannot be digested.
    pub fn new(
        descriptor: &ResolvedModelDescriptor,
        runtime: &RuntimeModelAuthority,
        semantic_evidence: ProviderSemanticEvidence,
    ) -> Result<Self, ProviderConfigurationError> {
        if runtime.descriptor_binding()
            != descriptor
                .binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?
        {
            return Err(ProviderConfigurationError::ForeignAuthority);
        }
        Self::from_bindings(
            descriptor
                .semantic_binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?,
            runtime.account_binding(),
            runtime.credential_authority_binding(),
            semantic_evidence,
        )
    }

    fn from_bindings(
        semantic_descriptor_binding: Digest,
        account_binding: Digest,
        credential_authority_binding: Digest,
        semantic_evidence: ProviderSemanticEvidence,
    ) -> Result<Self, ProviderConfigurationError> {
        if semantic_descriptor_binding.domain() != DigestDomain::ModelBinding
            || account_binding.domain() != DigestDomain::ModelBinding
            || credential_authority_binding.domain() != DigestDomain::ModelBinding
        {
            return Err(ProviderConfigurationError::ForeignAuthority);
        }
        let identity = Digest::sha256(
            DigestDomain::ProviderAuthorityBinding,
            1,
            &[
                b"plakous/resolved-provider-authority-v1",
                &semantic_descriptor_binding.identity_bytes(),
                &account_binding.identity_bytes(),
                &credential_authority_binding.identity_bytes(),
                &semantic_evidence.identity().identity_bytes(),
            ],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(Self {
            semantic_descriptor_binding,
            account_binding,
            credential_authority_binding,
            semantic_evidence,
            identity,
        })
    }

    /// Returns the stable semantic descriptor binding.
    #[must_use]
    pub const fn semantic_descriptor_binding(&self) -> Digest {
        self.semantic_descriptor_binding
    }
    /// Returns the stable selected-account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }
    /// Returns the stable credential-authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }
    /// Returns complete provider-owned semantic evidence.
    #[must_use]
    pub const fn semantic_evidence(&self) -> &ProviderSemanticEvidence {
        &self.semantic_evidence
    }
    /// Returns the stable authority identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Explicit requested reasoning effort.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReasoningEffortDisposition {
    /// Reasoning is disabled.
    Disabled,
    /// One exact effort is required.
    Enabled(ReasoningEffort),
}

/// Explicit reasoning-summary behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReasoningSummaryDisposition {
    /// Omit a summary request.
    Omit,
    /// Request the family-defined automatic summary.
    Auto,
    /// Request a concise summary.
    Concise,
    /// Request a detailed summary.
    Detailed,
}

/// Explicit native reasoning-material inclusion behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReasoningIncludeDisposition {
    /// Omit a native reasoning-material selector.
    Omit,
    /// Request encrypted native reasoning material.
    EncryptedContent,
}

/// Explicit continuation and replay behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReplayDisposition {
    /// Do not use native continuation or opaque replay.
    Disabled,
    /// Require replayable encrypted reasoning material when reasoning is produced.
    StrictEncrypted,
    /// Preserve replayable material but permit dropping unreplayable reasoning.
    DropUnreplayableReasoning,
}

/// Where one session sits in a consumer's agent topology.
///
/// Plakous itself is single agent; this only tells a provider dialect whether
/// the consumer spawned the session from another agent. Lineage is not part of
/// any request or configuration identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionLineage {
    /// The consumer's top-level session.
    #[default]
    Root,
    /// A session another agent spawned through the consumer.
    SpawnedChild,
}

/// Exact provider request processing/service-tier selection.
///
/// `Standard` is a semantic selection whose provider wire representation is
/// omission. A named tier retains the exact provider-owned label without
/// giving the neutral model knowledge of its meaning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProcessingTierDisposition {
    /// Use standard processing and omit a provider service-tier field.
    Standard,
    /// Request one exact provider-named tier.
    ProviderNamed(ProviderServiceTierId),
}

/// Where a configured tool executes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolExecutionDisposition {
    /// Execute through the caller-supplied local tool registry.
    Local,
    /// Execute through a provider-hosted tool contract.
    ProviderHosted,
}

/// Which caller lineages may invoke one configured function tool.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolCallerDisposition {
    /// Only a direct model call is admitted.
    DirectOnly,
    /// Only a provider-hosted programmatic caller is admitted.
    ProviderProgrammaticOnly,
    /// Direct and provider-hosted programmatic callers are both admitted.
    DirectAndProviderProgrammatic,
}

/// One exact exposed tool and its execution owner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredTool {
    definition: ToolDefinition,
    execution: ToolExecutionDisposition,
    caller: ToolCallerDisposition,
    /// The MCP server that executes a locally dispatched tool. Absent for
    /// every other tool, so their encoding and identity omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_server: Option<McpServerId>,
}

impl ConfiguredTool {
    /// Creates one configured tool.
    #[must_use]
    pub const fn new(
        definition: ToolDefinition,
        execution: ToolExecutionDisposition,
        caller: ToolCallerDisposition,
    ) -> Self {
        Self {
            definition,
            execution,
            caller,
            mcp_server: None,
        }
    }

    /// Marks a locally dispatched tool as executed by one MCP server, so
    /// protocols decode its calls with that owner from the first parse.
    ///
    /// # Errors
    ///
    /// Refuses a provider-hosted tool, whose owner is the provider family.
    pub fn with_mcp_owner(mut self, server: McpServerId) -> Result<Self, ToolOwnerProjectionError> {
        if self.execution != ToolExecutionDisposition::Local {
            return Err(ToolOwnerProjectionError::OwnerMismatch);
        }
        self.mcp_server = Some(server);
        Ok(self)
    }

    /// Returns the runtime owner of a locally dispatched tool: its MCP
    /// server, or the local runtime. `None` for a provider-hosted tool.
    #[must_use]
    pub fn local_owner(&self) -> Option<ToolExecutionOwner> {
        match self.execution {
            ToolExecutionDisposition::Local => Some(match &self.mcp_server {
                Some(server) => ToolExecutionOwner::Mcp {
                    server: server.clone(),
                },
                None => ToolExecutionOwner::Local,
            }),
            ToolExecutionDisposition::ProviderHosted => None,
        }
    }

    /// Returns the canonical model-facing definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Returns the execution owner.
    #[must_use]
    pub const fn execution(&self) -> ToolExecutionDisposition {
        self.execution
    }

    /// Returns the admitted caller lineages.
    #[must_use]
    pub const fn caller(&self) -> ToolCallerDisposition {
        self.caller
    }

    /// Validates one exact runtime-owner projection without changing ownership.
    ///
    /// Caller lineage is independent of execution ownership: both direct and
    /// provider-programmatic calls may retain a Plak-local or Plak MCP owner,
    /// while provider-hosted operations must retain family ownership. A tool
    /// configured with an MCP owner admits only that server.
    ///
    /// # Errors
    ///
    /// Returns an error when the projected owner disagrees with configuration.
    pub fn validate_owner_projection(
        &self,
        owner: &ToolExecutionOwner,
    ) -> Result<(), ToolOwnerProjectionError> {
        if !matches!(
            (self.execution, owner),
            (
                ToolExecutionDisposition::Local,
                ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. },
            ) | (
                ToolExecutionDisposition::ProviderHosted,
                ToolExecutionOwner::ProviderHosted { .. },
            )
        ) {
            return Err(ToolOwnerProjectionError::OwnerMismatch);
        }
        if let Some(configured) = &self.mcp_server
            && !matches!(owner, ToolExecutionOwner::Mcp { server } if server == configured)
        {
            return Err(ToolOwnerProjectionError::OwnerMismatch);
        }
        Ok(())
    }
}

/// Closed failure from coupling caller eligibility to one exact execution owner.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolOwnerProjectionError {
    /// Configuration and the family projection name different execution classes.
    #[error("configured tool execution disagrees with its projected owner")]
    OwnerMismatch,
}

/// One identity-bearing provider-hosted tool selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum HostedTool {
    /// Provider-hosted code that may call explicitly eligible configured tools.
    ProgrammaticComposition,
    /// Provider-hosted code execution without customer-tool dispatch authority.
    CodeInterpreter,
}

/// Explicit parallel-tool behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ParallelToolDisposition {
    /// Parallel model tool calls are disabled.
    Disabled,
    /// Parallel model tool calls are enabled.
    Enabled,
}

/// Explicit provider request output-bound behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OutputBoundDisposition {
    /// The route has no provider-enforced per-request bound available to the harness.
    NoRequestBound,
    /// Emit one exact provider-enforced request bound.
    ///
    /// The product chose this figure and it does not move on its own, so a
    /// session that reopens with a different one has had its output contract
    /// changed and is refused.
    Enforced(NonZeroU32),
    /// Emit one bound the product derived from the route's own attestation.
    ///
    /// The figure is not a product choice: it follows what the route's
    /// endpoints attest, so it moves when the provider's catalog moves. A
    /// session pinned to the figure that was current when it was created would
    /// become unresumable the day upstream changed, which is why reopen
    /// re-derives it rather than demanding the persisted one. The change is
    /// recorded and surfaced rather than applied silently.
    DerivedFromRoute(NonZeroU32),
}

impl OutputBoundDisposition {
    /// The bound to send, whichever way it was arrived at.
    #[must_use]
    pub const fn bound(self) -> Option<NonZeroU32> {
        match self {
            Self::NoRequestBound => None,
            Self::Enforced(bound) | Self::DerivedFromRoute(bound) => Some(bound),
        }
    }

    /// Reports whether the product derived this bound from the route.
    ///
    /// Reopen reads this to decide whether a differing bound is drift to refuse
    /// or an upstream figure to re-derive.
    #[must_use]
    pub const fn is_derived_from_route(self) -> bool {
        matches!(self, Self::DerivedFromRoute(_))
    }
}

/// Explicit provider-side response storage behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderStorageDisposition {
    /// Emit the route's explicit storage-disabled encoding.
    DisabledOnWire,
    /// Omit the field under the named immutable route contract.
    OmittedRouteContract,
    /// Request provider-stored response state.
    ProviderStored,
}

/// Explicit provider retention authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetentionDisposition {
    /// Retention is governed by one immutable referenced route contract.
    RouteContract {
        /// Contract identifier.
        profile: ModelAdapterProfileId,
        /// Exact contract revision.
        revision: ProviderProfileRevisionId,
    },
}

/// Explicit provider response-state behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ResponseStateDisposition {
    /// Reconstruct subsequent requests from local canonical state and replay material.
    Stateless,
    /// Continue from provider-stored response state.
    ProviderStored,
}

/// Explicit local durability behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalDurabilityDisposition {
    /// The agent is deliberately ephemeral.
    Ephemeral,
    /// The agent requests durable publication but permits a stronger backend boundary.
    BestEffort,
    /// Every admitted turn requires reconstructive durable checkpoints.
    DurableRequired,
}

/// Exact non-secret route, profile, credential, and model selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRouteSelection {
    protocol: ProtocolId,
    route: RouteId,
    integration_profile: ModelAdapterProfileId,
    integration_revision: ProviderProfileRevisionId,
    auth_profile: ModelAdapterProfileId,
    auth_revision: ProviderProfileRevisionId,
    authority_class: ProviderAuthorityClassId,
    credential_selection: CredentialSelectionId,
    requested_model: ModelId,
}

impl ProviderRouteSelection {
    /// Creates one exact provider route selection. All arguments are already validated IDs.
    #[allow(
        clippy::too_many_arguments,
        reason = "the route selection is a closed authority tuple"
    )]
    #[must_use]
    pub const fn new(
        protocol: ProtocolId,
        route: RouteId,
        integration_profile: ModelAdapterProfileId,
        integration_revision: ProviderProfileRevisionId,
        auth_profile: ModelAdapterProfileId,
        auth_revision: ProviderProfileRevisionId,
        authority_class: ProviderAuthorityClassId,
        credential_selection: CredentialSelectionId,
        requested_model: ModelId,
    ) -> Self {
        Self {
            protocol,
            route,
            integration_profile,
            integration_revision,
            auth_profile,
            auth_revision,
            authority_class,
            credential_selection,
            requested_model,
        }
    }

    /// Returns the exact protocol family/dialect selection.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }
    /// Returns the exact route selection.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }
    /// Returns the exact integration profile.
    #[must_use]
    pub const fn integration_profile(&self) -> &ModelAdapterProfileId {
        &self.integration_profile
    }
    /// Returns the exact integration-profile revision.
    #[must_use]
    pub const fn integration_revision(&self) -> &ProviderProfileRevisionId {
        &self.integration_revision
    }
    /// Returns the exact authentication profile.
    #[must_use]
    pub const fn auth_profile(&self) -> &ModelAdapterProfileId {
        &self.auth_profile
    }
    /// Returns the exact authentication-profile revision.
    #[must_use]
    pub const fn auth_revision(&self) -> &ProviderProfileRevisionId {
        &self.auth_revision
    }
    /// Returns the authority/billing class.
    #[must_use]
    pub const fn authority_class(&self) -> &ProviderAuthorityClassId {
        &self.authority_class
    }
    /// Returns the explicit non-secret credential selection.
    #[must_use]
    pub const fn credential_selection(&self) -> &CredentialSelectionId {
        &self.credential_selection
    }
    /// Returns the literal requested model.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }
}

/// Exact provider-neutral model-facing tool-surface mode.
///
/// This identity-bearing selection distinguishes ordinary direct tools from a
/// local programmatic-composition surface without naming a provider, language,
/// interpreter, or presentation grammar. Exact provider admission still owns
/// the mapping from authenticated evidence to one of these closed meanings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationMode {
    /// Expose only ordinary direct tools.
    #[default]
    Direct,
    /// Expose ordinary direct tools plus local programmatic composition.
    LocalComposition,
    /// Expose only local programmatic composition controls.
    LocalCompositionOnly,
}

/// Complete explicit provider-neutral behavior selection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBehaviorConfiguration {
    /// Exact provider processing/service tier.
    pub processing_tier: ProcessingTierDisposition,
    /// Exact model-facing tool-surface mode.
    pub tool_mode: ToolPresentationMode,
    /// Exact reasoning effort.
    pub reasoning_effort: ReasoningEffortDisposition,
    /// Exact reasoning summary.
    pub reasoning_summary: ReasoningSummaryDisposition,
    /// Exact reasoning material inclusion.
    pub reasoning_include: ReasoningIncludeDisposition,
    /// Exact replay behavior.
    pub replay: ReplayDisposition,
    /// Exact prompt-cache behavior.
    pub cache: CachePolicy,
    /// Exact exposed tools and execution owners.
    pub tools: Vec<ConfiguredTool>,
    /// Exact provider-hosted tools, separate from function definitions.
    pub hosted_tools: Vec<HostedTool>,
    /// Exact model tool choice.
    pub tool_choice: ToolChoice,
    /// Exact parallel-tool behavior.
    pub parallel_tools: ParallelToolDisposition,
    /// Exact output format.
    pub output: OutputPolicy,
    /// Exact provider request output bound.
    pub output_bound: OutputBoundDisposition,
    /// Exact provider storage encoding.
    pub storage: ProviderStorageDisposition,
    /// Exact retention contract.
    pub retention: RetentionDisposition,
    /// Exact provider response-state behavior.
    pub response_state: ResponseStateDisposition,
    /// Minimum accepted context evidence class.
    pub context_evidence_requirement: ContextAdmissionRequirement,
    /// Exact local durability behavior.
    pub local_durability: LocalDurabilityDisposition,
}

impl ProviderBehaviorConfiguration {
    /// Checks exact request tool choice and output bound, allowing the bounded
    /// session-workflow refinement used for harness semantic compaction and the
    /// harness wrap-up round that disables tool selection.
    ///
    /// A workflow may disable tool selection and request exactly
    /// [`semantic_compaction_output_bound`] of the configured bound and the
    /// request's reasoning; a route without a request bound admits none.
    /// A request whose configuration leaves tool selection to the model
    /// ([`ToolChoice::Auto`]) may instead lower its tool choice to
    /// [`ToolChoice::None`] while keeping the exact configured output bound:
    /// withdrawing a free choice only narrows what the round may do, so it
    /// needs no separate configuration authority. A configuration that *forces*
    /// a call ([`ToolChoice::Required`]) or names one
    /// ([`ToolChoice::Specific`]) is a contract about the round's output shape,
    /// and a request may not withdraw from it. This does not authorize changes
    /// to configuration identities, tools, reasoning, replay, or any other
    /// request field; adapters must check those.
    #[must_use]
    pub fn matches_request_execution_controls(&self, request: &crate::ModelRequest) -> bool {
        if !request.limits().stop_sequences().is_empty() {
            return false;
        }
        let exact_bound = request.limits().max_output_tokens() == self.output_bound.bound();
        if request.tool_choice() == &self.tool_choice && exact_bound {
            return true;
        }
        if request.tool_choice() == &ToolChoice::None
            && matches!(self.tool_choice, ToolChoice::Auto | ToolChoice::None)
            && exact_bound
        {
            return true;
        }
        matches!(
            request.metadata().root(),
            crate::ModelOperationRoot::SessionWorkflow(_)
        ) && request.tool_choice() == &ToolChoice::None
            && self.output_bound.bound().is_some_and(|configured| {
                request.limits().max_output_tokens()
                    == Some(semantic_compaction_output_bound(
                        Some(configured),
                        request.reasoning(),
                    ))
            })
    }
}

/// A complete caller-selected provider configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SelectedProviderConfiguration {
    route: ProviderRouteSelection,
    behavior: ProviderBehaviorConfiguration,
    schema_revision: u32,
    identity: SelectedProviderConfigurationId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedProviderConfigurationWire {
    route: ProviderRouteSelection,
    behavior: ProviderBehaviorConfiguration,
    schema_revision: u32,
    identity: SelectedProviderConfigurationId,
}

impl<'de> Deserialize<'de> for SelectedProviderConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SelectedProviderConfigurationWire::deserialize(deserializer)?;
        if wire.schema_revision != SELECTED_PROVIDER_CONFIGURATION_REVISION {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::UnsupportedRevision,
            ));
        }
        let value = Self::new(wire.route, wire.behavior).map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(value)
    }
}

impl SelectedProviderConfiguration {
    /// Validates and seals a complete selected configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate tools, unsatisfiable tool choice, or identity failure.
    pub fn new(
        route: ProviderRouteSelection,
        behavior: ProviderBehaviorConfiguration,
    ) -> Result<Self, ProviderConfigurationError> {
        validate_behavior_shape(&behavior)?;
        let bytes =
            serde_json::to_vec(&(&route, &behavior, SELECTED_PROVIDER_CONFIGURATION_REVISION))
                .map_err(|_| ProviderConfigurationError::Identity)?;
        let identity = SelectedProviderConfigurationId::new(configuration_digest(
            DigestDomain::SelectedProviderConfiguration,
            5,
            b"plakous/selected-provider-configuration-v5",
            &bytes,
        )?)?;
        Ok(Self {
            route,
            behavior,
            schema_revision: SELECTED_PROVIDER_CONFIGURATION_REVISION,
            identity,
        })
    }

    /// Returns the exact route selection.
    #[must_use]
    pub const fn route(&self) -> &ProviderRouteSelection {
        &self.route
    }
    /// Returns the complete selected behavior.
    #[must_use]
    pub const fn behavior(&self) -> &ProviderBehaviorConfiguration {
        &self.behavior
    }
    /// Returns the selected-configuration identity.
    #[must_use]
    pub const fn identity(&self) -> SelectedProviderConfigurationId {
        self.identity
    }
    /// Returns the selected schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }
}

/// Reconstruction-complete canonical family policy compiled by the owning kernel.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FamilyPolicyEnvelope {
    family: ModelFamilyId,
    schema_revision: u32,
    compiler_revision: u32,
    dialect_revision: u32,
    canonical_bytes: Vec<u8>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyPolicyEnvelopeWire {
    family: ModelFamilyId,
    schema_revision: u32,
    compiler_revision: u32,
    dialect_revision: u32,
    canonical_bytes: Vec<u8>,
    identity: Digest,
}

impl<'de> Deserialize<'de> for FamilyPolicyEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FamilyPolicyEnvelopeWire::deserialize(deserializer)?;
        let value = Self::new(
            wire.family,
            wire.schema_revision,
            wire.compiler_revision,
            wire.dialect_revision,
            wire.canonical_bytes,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(value)
    }
}

impl FamilyPolicyEnvelope {
    /// Seals one bounded canonical family policy.
    ///
    /// # Errors
    ///
    /// Returns an error for zero revisions, empty/oversized bytes, or identity failure.
    pub fn new(
        family: ModelFamilyId,
        schema_revision: u32,
        compiler_revision: u32,
        dialect_revision: u32,
        canonical_bytes: Vec<u8>,
    ) -> Result<Self, ProviderConfigurationError> {
        if schema_revision == 0 || compiler_revision == 0 || dialect_revision == 0 {
            return Err(ProviderConfigurationError::UnsupportedRevision);
        }
        if canonical_bytes.is_empty() || canonical_bytes.len() > MAX_FAMILY_POLICY_BYTES {
            return Err(ProviderConfigurationError::InvalidFamilyPolicy);
        }
        let revisions = [schema_revision, compiler_revision, dialect_revision]
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect::<Vec<_>>();
        let identity = Digest::sha256(
            DigestDomain::ModelBinding,
            7,
            &[
                b"plakous/family-policy-envelope-v1",
                family.as_str().as_bytes(),
                &revisions,
                &canonical_bytes,
            ],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(Self {
            family,
            schema_revision,
            compiler_revision,
            dialect_revision,
            canonical_bytes,
            identity,
        })
    }

    /// Returns the model family.
    #[must_use]
    pub const fn family(&self) -> &ModelFamilyId {
        &self.family
    }
    /// Returns the family schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }
    /// Returns the family compiler revision.
    #[must_use]
    pub const fn compiler_revision(&self) -> u32 {
        self.compiler_revision
    }
    /// Returns the dialect revision.
    #[must_use]
    pub const fn dialect_revision(&self) -> u32 {
        self.dialect_revision
    }
    /// Returns the complete canonical policy bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
    /// Returns the family policy identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// One complete effective provider configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EffectiveProviderConfiguration {
    selected_identity: SelectedProviderConfigurationId,
    descriptor: ResolvedModelDescriptor,
    model_implementation: ModelImplementationIdentity,
    authority: ResolvedProviderAuthorityBinding,
    behavior: ProviderBehaviorConfiguration,
    family_policy: FamilyPolicyEnvelope,
    compiler_revision: u32,
    identity: EffectiveProviderConfigurationId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveProviderConfigurationWire {
    selected_identity: SelectedProviderConfigurationId,
    descriptor: ResolvedModelDescriptor,
    model_implementation: ModelImplementationIdentity,
    authority: ResolvedProviderAuthorityBinding,
    behavior: ProviderBehaviorConfiguration,
    family_policy: FamilyPolicyEnvelope,
    compiler_revision: u32,
    identity: EffectiveProviderConfigurationId,
}

impl<'de> Deserialize<'de> for EffectiveProviderConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EffectiveProviderConfigurationWire::deserialize(deserializer)?;
        if wire.compiler_revision != EFFECTIVE_PROVIDER_CONFIGURATION_COMPILER_REVISION {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::UnsupportedRevision,
            ));
        }
        validate_integration_ownership(&wire.descriptor, &wire.model_implementation)
            .map_err(serde::de::Error::custom)?;
        let identity = effective_identity(
            wire.selected_identity,
            &wire.descriptor,
            &wire.model_implementation,
            &wire.authority,
            &wire.behavior,
            &wire.family_policy,
        )
        .map_err(serde::de::Error::custom)?;
        if identity != wire.identity {
            return Err(serde::de::Error::custom(
                ProviderConfigurationError::Integrity,
            ));
        }
        Ok(Self {
            selected_identity: wire.selected_identity,
            descriptor: wire.descriptor,
            model_implementation: wire.model_implementation,
            authority: wire.authority,
            behavior: wire.behavior,
            family_policy: wire.family_policy,
            compiler_revision: wire.compiler_revision,
            identity,
        })
    }
}

impl EffectiveProviderConfiguration {
    /// Validates and compiles one selected configuration against exact resolved facts.
    ///
    /// The owning family compiler must supply `behavior` and `family_policy`; this neutral
    /// compiler validates their generic consistency but never invents a family mapping.
    ///
    /// # Errors
    ///
    /// Returns an error for route, authority, capability, behavior, or identity mismatch.
    pub fn compile(
        selected: &SelectedProviderConfiguration,
        descriptor: ResolvedModelDescriptor,
        model_implementation: ModelImplementationIdentity,
        authority: ResolvedProviderAuthorityBinding,
        behavior: ProviderBehaviorConfiguration,
        family_policy: FamilyPolicyEnvelope,
    ) -> Result<Self, ProviderConfigurationError> {
        if behavior != *selected.behavior() {
            return Err(ProviderConfigurationError::UnsupportedBehavior);
        }
        if descriptor.model_family() != Some(family_policy.family()) {
            return Err(ProviderConfigurationError::ForeignTarget);
        }
        validate_resolution(
            selected,
            &descriptor,
            &model_implementation,
            &authority,
            &behavior,
        )?;
        let identity = effective_identity(
            selected.identity,
            &descriptor,
            &model_implementation,
            &authority,
            &behavior,
            &family_policy,
        )?;
        Ok(Self {
            selected_identity: selected.identity,
            descriptor,
            model_implementation,
            authority,
            behavior,
            family_policy,
            compiler_revision: EFFECTIVE_PROVIDER_CONFIGURATION_COMPILER_REVISION,
            identity,
        })
    }

    /// Returns the selected configuration identity.
    #[must_use]
    pub const fn selected_identity(&self) -> SelectedProviderConfigurationId {
        self.selected_identity
    }
    /// Returns the complete resolved descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
    /// Returns the exact executable implementation required for reconstruction.
    #[must_use]
    pub const fn model_implementation(&self) -> &ModelImplementationIdentity {
        &self.model_implementation
    }
    /// Returns complete credential-free runtime authority.
    #[must_use]
    pub const fn authority(&self) -> &ResolvedProviderAuthorityBinding {
        &self.authority
    }
    /// Returns exact effective behavior.
    #[must_use]
    pub const fn behavior(&self) -> &ProviderBehaviorConfiguration {
        &self.behavior
    }
    /// Returns the complete family policy envelope.
    #[must_use]
    pub const fn family_policy(&self) -> &FamilyPolicyEnvelope {
        &self.family_policy
    }
    /// Returns the effective model.
    #[must_use]
    pub fn effective_model(&self) -> &ModelId {
        self.descriptor
            .reported_model()
            .unwrap_or_else(|| self.descriptor.requested_model())
    }
    /// Returns the effective configuration identity.
    #[must_use]
    pub const fn identity(&self) -> EffectiveProviderConfigurationId {
        self.identity
    }
}

/// Complete selected and effective provider configuration authority.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledProviderConfiguration {
    selected: SelectedProviderConfiguration,
    effective: EffectiveProviderConfiguration,
}

/// Immutable validated descriptor-first binding to one executable implementation.
#[derive(Clone)]
pub struct RegisteredLanguageModel {
    descriptor: ResolvedModelDescriptor,
    implementation: ModelImplementationIdentity,
    authority: RegistrationAuthority,
    model: Arc<dyn LanguageModel>,
}

#[derive(Clone)]
enum RegistrationAuthority {
    Standard(StandardIntegrationAuthority),
    Custom,
}

impl RegisteredLanguageModel {
    /// Registers a caller-supplied executable under a custom descriptor snapshot.
    ///
    /// # Errors
    ///
    /// Rejects standard descriptors, standard evidence, or implementation mismatch.
    pub fn custom(
        descriptor: ResolvedModelDescriptor,
        implementation: ModelImplementationIdentity,
        model: Arc<dyn LanguageModel>,
    ) -> Result<Self, ProviderConfigurationError> {
        validate_integration_ownership(&descriptor, &implementation)?;
        if descriptor.integration_class() != crate::IntegrationClass::Custom {
            return Err(ProviderConfigurationError::ForeignIntegrationClass);
        }
        Ok(Self {
            descriptor,
            implementation,
            authority: RegistrationAuthority::Custom,
            model,
        })
    }

    /// Registers a release-owned maintained implementation.
    ///
    /// This constructor is implementation plumbing and must not be reexported by
    /// the curated custom-registration facade.
    #[doc(hidden)]
    pub fn standard(
        authority: &StandardIntegrationAuthority,
        descriptor: ResolvedModelDescriptor,
        implementation: ModelImplementationIdentity,
        model: Arc<dyn LanguageModel>,
    ) -> Result<Self, ProviderConfigurationError> {
        validate_integration_ownership(&descriptor, &implementation)?;
        if descriptor.integration_class() != crate::IntegrationClass::Standard {
            return Err(ProviderConfigurationError::ForeignIntegrationClass);
        }
        Ok(Self {
            descriptor,
            implementation,
            authority: RegistrationAuthority::Standard(authority.clone()),
            model,
        })
    }

    /// Returns the immutable registered descriptor snapshot.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
    /// Returns the executable implementation identity and provenance.
    #[must_use]
    pub const fn implementation(&self) -> &ModelImplementationIdentity {
        &self.implementation
    }
    /// Returns the executable without transferring descriptor authority to it.
    #[must_use]
    pub fn executable(&self) -> Arc<dyn LanguageModel> {
        Arc::clone(&self.model)
    }

    fn validates_runtime_authority(&self) -> bool {
        matches!(
            (&self.authority, self.descriptor.integration_class()),
            (
                RegistrationAuthority::Standard(_),
                crate::IntegrationClass::Standard
            ) | (
                RegistrationAuthority::Custom,
                crate::IntegrationClass::Custom
            )
        )
    }
}

impl std::fmt::Debug for RegisteredLanguageModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredLanguageModel")
            .field("descriptor", &self.descriptor)
            .field("implementation", &self.implementation)
            .finish_non_exhaustive()
    }
}

/// Sealed aggregate joining a registered executable to exact configuration and authority.
#[derive(Clone)]
pub struct ConfiguredLanguageModel {
    registered: RegisteredLanguageModel,
    configuration: CompiledProviderConfiguration,
    initial_authority: RuntimeModelAuthority,
}

impl ConfiguredLanguageModel {
    /// Joins provider construction outputs after cross-validating every shared
    /// descriptor and stable authority axis.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable descriptor, compiled descriptor,
    /// or fresh/stable authority bindings disagree.
    pub fn from_registered(
        registered: RegisteredLanguageModel,
        configuration: CompiledProviderConfiguration,
        initial_authority: RuntimeModelAuthority,
    ) -> Result<Self, ProviderConfigurationError> {
        let effective = configuration.effective();
        if !registered.validates_runtime_authority()
            || registered.descriptor() != effective.descriptor()
            || registered.implementation() != effective.model_implementation()
        {
            return Err(ProviderConfigurationError::ForeignTarget);
        }
        let descriptor_binding = registered
            .descriptor()
            .binding_digest()
            .map_err(|_| ProviderConfigurationError::Identity)?;
        let stable = effective.authority();
        if initial_authority.descriptor_binding() != descriptor_binding
            || initial_authority.account_binding() != stable.account_binding()
            || initial_authority.credential_authority_binding()
                != stable.credential_authority_binding()
        {
            return Err(ProviderConfigurationError::ForeignAuthority);
        }
        Ok(Self {
            registered,
            configuration,
            initial_authority,
        })
    }

    /// Binds one maintained release-owned executable to its compiled configuration.
    ///
    /// This constructor is implementation plumbing and must not be reexported by
    /// the curated custom-registration facade.
    #[doc(hidden)]
    pub fn new(
        authority: &StandardIntegrationAuthority,
        model: Arc<dyn LanguageModel>,
        configuration: CompiledProviderConfiguration,
        initial_authority: RuntimeModelAuthority,
    ) -> Result<Self, ProviderConfigurationError> {
        let effective = configuration.effective();
        let registered = RegisteredLanguageModel::standard(
            authority,
            effective.descriptor().clone(),
            effective.model_implementation().clone(),
            model,
        )?;
        Self::from_registered(registered, configuration, initial_authority)
    }

    /// Registers a caller executable in the custom integration lane.
    ///
    /// The descriptor snapshot and implementation identity come from the exact
    /// compiled configuration; this path cannot mint `Standard` or release-owned evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when the compiled descriptor is not `Custom`, its
    /// implementation identity differs, or its initial authority is foreign.
    pub fn custom(
        model: Arc<dyn LanguageModel>,
        configuration: CompiledProviderConfiguration,
        initial_authority: RuntimeModelAuthority,
    ) -> Result<Self, ProviderConfigurationError> {
        let effective = configuration.effective();
        let registered = RegisteredLanguageModel::custom(
            effective.descriptor().clone(),
            effective.model_implementation().clone(),
            model,
        )?;
        Self::from_registered(registered, configuration, initial_authority)
    }

    /// Returns the exact executable model.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        self.registered.executable()
    }
    /// Returns the immutable registered descriptor and implementation binding.
    #[must_use]
    pub const fn registered(&self) -> &RegisteredLanguageModel {
        &self.registered
    }
    /// Returns the complete selected/effective configuration.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
    /// Returns the fresh authority captured during resolution.
    #[must_use]
    pub const fn initial_authority(&self) -> &RuntimeModelAuthority {
        &self.initial_authority
    }
}

impl std::fmt::Debug for ConfiguredLanguageModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredLanguageModel")
            .field("descriptor", self.registered.descriptor())
            .field("implementation", self.registered.implementation())
            .field(
                "selected_configuration",
                &self.configuration.selected_identity(),
            )
            .field(
                "effective_configuration",
                &self.configuration.effective_identity(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompiledProviderConfigurationWire {
    selected: SelectedProviderConfiguration,
    effective: EffectiveProviderConfiguration,
}

impl<'de> Deserialize<'de> for CompiledProviderConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CompiledProviderConfigurationWire::deserialize(deserializer)?;
        Self::new(wire.selected, wire.effective).map_err(serde::de::Error::custom)
    }
}

impl CompiledProviderConfiguration {
    /// Creates and cross-validates one compiled configuration aggregate.
    ///
    /// # Errors
    ///
    /// Returns an error when the effective value belongs to another selection.
    pub fn new(
        selected: SelectedProviderConfiguration,
        effective: EffectiveProviderConfiguration,
    ) -> Result<Self, ProviderConfigurationError> {
        if effective.selected_identity != selected.identity {
            return Err(ProviderConfigurationError::ForeignSelection);
        }
        if effective.behavior != selected.behavior {
            return Err(ProviderConfigurationError::UnsupportedBehavior);
        }
        if effective.descriptor.model_family() != Some(effective.family_policy.family()) {
            return Err(ProviderConfigurationError::ForeignTarget);
        }
        validate_resolution(
            &selected,
            &effective.descriptor,
            &effective.model_implementation,
            &effective.authority,
            &effective.behavior,
        )?;
        Ok(Self {
            selected,
            effective,
        })
    }

    /// Recompiles this configuration as the driver-owned no-tools compact operation.
    ///
    /// Live Compact must not use this path. Product Compact keeps the
    /// coding runtime and a trailing-user instruction. This helper remains for
    /// offline compiler tests of the retired tools-off / cache-off shape.
    ///
    /// Route, descriptor, authority, family policy, processing tier, storage,
    /// retention, and context-evidence class stay exact. Tools, reasoning,
    /// cache, and replay are disabled; the output bound is
    /// [`SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the compaction behavior cannot be sealed against
    /// this source configuration.
    pub fn semantic_compaction(&self) -> Result<Self, ProviderConfigurationError> {
        let source = self.effective.behavior();
        let behavior = ProviderBehaviorConfiguration {
            processing_tier: source.processing_tier.clone(),
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS),
            storage: source.storage,
            retention: source.retention.clone(),
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: source.context_evidence_requirement,
            local_durability: source.local_durability,
        };
        let selected =
            SelectedProviderConfiguration::new(self.selected.route.clone(), behavior.clone())?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            self.effective.descriptor.clone(),
            self.effective.model_implementation.clone(),
            self.effective.authority.clone(),
            behavior,
            self.effective.family_policy.clone(),
        )?;
        Self::new(selected, effective)
    }

    /// Returns the complete selected configuration.
    #[must_use]
    pub const fn selected(&self) -> &SelectedProviderConfiguration {
        &self.selected
    }
    /// Returns the complete effective configuration.
    #[must_use]
    pub const fn effective(&self) -> &EffectiveProviderConfiguration {
        &self.effective
    }
    /// Returns the selected configuration identity.
    #[must_use]
    pub const fn selected_identity(&self) -> SelectedProviderConfigurationId {
        self.selected.identity
    }
    /// The identity pair a persisted record must be compared against on reopen.
    ///
    /// The runtime manifest is the one gate that asks this, and it asks it of
    /// the persisted prefix and the persisted harness plan together. Every
    /// other reopen comparison is exact, because a reopened session is compiled
    /// with the bound it was created under and so carries the same identity it
    /// persisted. What remains for this is a session created under a
    /// product-chosen figure on a route that has since started deriving one. A
    /// route that
    /// derives its bound from its own attestation moves that figure when the
    /// provider's catalog moves, and the figure is identity-bearing, so the
    /// persisted pair is admitted when it is the pair this very configuration
    /// would carry under the bound the session was created under. Everything
    /// else still has to match exactly. A route whose bound the product chose
    /// keeps the exact comparison, so a changed bound there is still drift.
    ///
    /// A session created before the route derived its bound persisted the
    /// figure as [`OutputBoundDisposition::Enforced`]; one created after
    /// persisted it as [`OutputBoundDisposition::DerivedFromRoute`]. Both name
    /// the same session on the same route, so both spellings are admitted.
    ///
    /// Returns this configuration's own pair when nothing is substituted, which
    /// is what an exact comparison then refuses.
    ///
    /// # Errors
    ///
    /// Returns an identity error when a substituted configuration cannot be
    /// digested.
    pub fn reopen_identities(
        &self,
        persisted_bound: Option<NonZeroU32>,
        persisted: (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
    ) -> Result<
        (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
        ProviderConfigurationError,
    > {
        let live = (self.selected_identity(), self.effective_identity());
        if !self
            .effective()
            .behavior()
            .output_bound
            .is_derived_from_route()
        {
            return Ok(live);
        }
        let candidates = match persisted_bound {
            None => vec![OutputBoundDisposition::NoRequestBound],
            Some(bound) => vec![
                OutputBoundDisposition::DerivedFromRoute(bound),
                OutputBoundDisposition::Enforced(bound),
            ],
        };
        for candidate in candidates {
            let substituted = self.identities_with_output_bound(candidate)?;
            if substituted == persisted {
                return Ok(substituted);
            }
        }
        Ok(live)
    }

    /// The identity pair this configuration would carry with a different
    /// product-derived output bound.
    ///
    /// A bound the product derives from a route's own attestation moves when
    /// the provider's catalog moves, and it is identity-bearing. Reopen uses
    /// this to ask the only question that matters: with the bound the session
    /// was created under, is everything else — route, model, authority,
    /// reasoning, tools, policy — still exactly the same? If it is, the session
    /// is the same session and the new bound is upstream news, not drift.
    ///
    /// The disposition is supplied rather than assumed, because a session
    /// created before the route derived its bound persisted it as
    /// [`OutputBoundDisposition::Enforced`], and that session has to reopen
    /// too.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the substituted configuration cannot be
    /// digested.
    pub fn identities_with_output_bound(
        &self,
        output_bound: OutputBoundDisposition,
    ) -> Result<
        (
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        ),
        ProviderConfigurationError,
    > {
        let mut behavior = self.selected.behavior.clone();
        behavior.output_bound = output_bound;
        let bytes = serde_json::to_vec(&(
            &self.selected.route,
            &behavior,
            SELECTED_PROVIDER_CONFIGURATION_REVISION,
        ))
        .map_err(|_| ProviderConfigurationError::Identity)?;
        let selected = SelectedProviderConfigurationId::new(configuration_digest(
            DigestDomain::SelectedProviderConfiguration,
            5,
            b"plakous/selected-provider-configuration-v5",
            &bytes,
        )?)?;
        let effective = effective_identity(
            selected,
            &self.effective.descriptor,
            &self.effective.model_implementation,
            &self.effective.authority,
            &behavior,
            &self.effective.family_policy,
        )?;
        Ok((selected, effective))
    }

    /// Returns the effective configuration identity.
    #[must_use]
    pub const fn effective_identity(&self) -> EffectiveProviderConfigurationId {
        self.effective.identity
    }

    /// Seals fresh adapter authority to this exact compiled configuration.
    ///
    /// # Errors
    ///
    /// Rejects descriptor, semantic/catalog, account, credential-authority,
    /// freshness, or effective-configuration disagreement.
    pub fn seal_runtime_authority(
        &self,
        descriptor: &ResolvedModelDescriptor,
        authority: RuntimeModelAuthority,
        checked_at: Timestamp,
    ) -> Result<ConfiguredRuntimeAuthority, ProviderConfigurationError> {
        authority
            .validate(descriptor, checked_at)
            .map_err(|_| ProviderConfigurationError::ForeignAuthority)?;
        let effective = self.effective();
        let stable = effective.authority();
        if descriptor != effective.descriptor()
            || descriptor
                .semantic_binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?
                != stable.semantic_descriptor_binding()
            || authority.account_binding() != stable.account_binding()
            || authority.credential_authority_binding() != stable.credential_authority_binding()
        {
            return Err(ProviderConfigurationError::ForeignAuthority);
        }
        let binding = Digest::sha256(
            DigestDomain::ModelBinding,
            6,
            &[
                b"plakous-model/configured-runtime-authority-v1",
                &authority
                    .execution_authority_binding()
                    .map_err(|_| ProviderConfigurationError::Identity)?
                    .identity_bytes(),
                &self.effective_identity().digest().identity_bytes(),
                &stable.semantic_descriptor_binding().identity_bytes(),
            ],
        )
        .map_err(|_| ProviderConfigurationError::Identity)?;
        Ok(ConfiguredRuntimeAuthority {
            effective_configuration: self.effective_identity(),
            runtime: authority,
            binding,
        })
    }
}

/// Provider configuration construction or validation failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ProviderConfigurationError {
    /// A schema, compiler, or dialect revision is zero or unsupported.
    #[error("provider configuration revision is unsupported")]
    UnsupportedRevision,
    /// A family policy payload is empty or exceeds its bound.
    #[error("provider family policy is invalid")]
    InvalidFamilyPolicy,
    /// Provider semantic evidence is empty, oversized, or has an invalid revision.
    #[error("provider semantic evidence is invalid")]
    InvalidSemanticEvidence,
    /// Exposed tools or tool choice are structurally invalid.
    #[error("provider tool configuration is invalid")]
    InvalidTools,
    /// Resolved route/profile/model facts do not match the selection.
    #[error("resolved provider target does not match the selected target")]
    ForeignTarget,
    /// Fresh runtime authority belongs to another descriptor.
    #[error("runtime provider authority belongs to another descriptor")]
    ForeignAuthority,
    /// Effective behavior is not admitted by resolved capabilities.
    #[error("effective provider behavior is unsupported")]
    UnsupportedBehavior,
    /// Effective configuration belongs to another selected configuration.
    #[error("effective provider configuration belongs to another selection")]
    ForeignSelection,
    /// Integration class crossed the standard/custom registration boundary.
    #[error("model registration uses a foreign integration class")]
    ForeignIntegrationClass,
    /// Deterministic identity construction failed.
    #[error("provider configuration identity construction failed")]
    Identity,
    /// Serialized content does not match its identity.
    #[error("provider configuration integrity mismatch")]
    Integrity,
}

fn configuration_digest(
    domain: DigestDomain,
    version: u32,
    label: &[u8],
    bytes: &[u8],
) -> Result<Digest, ProviderConfigurationError> {
    Digest::sha256(domain, version, &[label, bytes])
        .map_err(|_| ProviderConfigurationError::Identity)
}

fn effective_identity(
    selected_identity: SelectedProviderConfigurationId,
    descriptor: &ResolvedModelDescriptor,
    model_implementation: &ModelImplementationIdentity,
    authority: &ResolvedProviderAuthorityBinding,
    behavior: &ProviderBehaviorConfiguration,
    family_policy: &FamilyPolicyEnvelope,
) -> Result<EffectiveProviderConfigurationId, ProviderConfigurationError> {
    let semantic_descriptor = descriptor
        .semantic_binding_digest()
        .map_err(|_| ProviderConfigurationError::Identity)?;
    let bytes = serde_json::to_vec(&(
        selected_identity,
        semantic_descriptor,
        model_implementation,
        authority,
        behavior,
        family_policy,
        EFFECTIVE_PROVIDER_CONFIGURATION_COMPILER_REVISION,
    ))
    .map_err(|_| ProviderConfigurationError::Identity)?;
    EffectiveProviderConfigurationId::new(configuration_digest(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        b"plakous/effective-provider-configuration-v5",
        &bytes,
    )?)
}

fn validate_behavior_shape(
    behavior: &ProviderBehaviorConfiguration,
) -> Result<(), ProviderConfigurationError> {
    let mut names = BTreeSet::new();
    if behavior
        .tools
        .iter()
        .any(|tool| !names.insert(tool.definition.name()))
    {
        return Err(ProviderConfigurationError::InvalidTools);
    }
    let mut hosted_tools = behavior.hosted_tools.clone();
    hosted_tools.sort_unstable();
    if hosted_tools.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ProviderConfigurationError::InvalidTools);
    }
    let programmatic_selected = hosted_tools.contains(&HostedTool::ProgrammaticComposition);
    let has_programmatic_caller = behavior
        .tools
        .iter()
        .any(|tool| !matches!(tool.caller(), ToolCallerDisposition::DirectOnly));
    if programmatic_selected != has_programmatic_caller {
        return Err(ProviderConfigurationError::InvalidTools);
    }
    match &behavior.tool_choice {
        ToolChoice::None if !behavior.tools.is_empty() => {}
        ToolChoice::Specific(name) if !names.contains(name) => {
            return Err(ProviderConfigurationError::InvalidTools);
        }
        _ => {}
    }
    if matches!(behavior.parallel_tools, ParallelToolDisposition::Enabled)
        && behavior.tools.is_empty()
    {
        return Err(ProviderConfigurationError::InvalidTools);
    }
    Ok(())
}

fn supports(descriptor: &ResolvedModelDescriptor, capability: Capability) -> bool {
    descriptor
        .capabilities()
        .get(capability)
        .is_some_and(|fact| matches!(fact.support(), Support::Native | Support::Emulated))
}

fn validate_resolution(
    selected: &SelectedProviderConfiguration,
    descriptor: &ResolvedModelDescriptor,
    model_implementation: &ModelImplementationIdentity,
    authority: &ResolvedProviderAuthorityBinding,
    behavior: &ProviderBehaviorConfiguration,
) -> Result<(), ProviderConfigurationError> {
    validate_integration_ownership(descriptor, model_implementation)?;
    validate_behavior_shape(behavior)?;
    let route = selected.route();
    if route.protocol() != descriptor.protocol()
        || route.route() != descriptor.route()
        || route.integration_profile() != descriptor.profile()
        || route.requested_model() != descriptor.requested_model()
    {
        return Err(ProviderConfigurationError::ForeignTarget);
    }
    let binding = descriptor
        .semantic_binding_digest()
        .map_err(|_| ProviderConfigurationError::Identity)?;
    if authority.semantic_descriptor_binding() != binding {
        return Err(ProviderConfigurationError::ForeignAuthority);
    }
    if matches!(
        behavior.reasoning_effort,
        ReasoningEffortDisposition::Enabled(_)
    ) && !supports(descriptor, Capability::Reasoning)
        || !behavior.tools.is_empty() && !supports(descriptor, Capability::Tools)
        || behavior
            .hosted_tools
            .contains(&HostedTool::ProgrammaticComposition)
            && !supports(descriptor, Capability::HostedProgrammaticToolCalling)
        || behavior.hosted_tools.contains(&HostedTool::CodeInterpreter)
            && !supports(descriptor, Capability::HostedCodeExecution)
        || matches!(behavior.parallel_tools, ParallelToolDisposition::Enabled)
            && !supports(descriptor, Capability::ParallelTools)
        || matches!(behavior.output.format(), OutputFormat::Structured(_))
            && !supports(descriptor, Capability::StructuredOutput)
        || !matches!(behavior.cache, CachePolicy::Disabled)
            && !supports(descriptor, Capability::Caching)
    {
        return Err(ProviderConfigurationError::UnsupportedBehavior);
    }
    Ok(())
}

fn validate_integration_ownership(
    descriptor: &ResolvedModelDescriptor,
    model_implementation: &ModelImplementationIdentity,
) -> Result<(), ProviderConfigurationError> {
    descriptor
        .validate_integration_class()
        .map_err(|_| ProviderConfigurationError::ForeignIntegrationClass)?;
    if descriptor.integration_class() != model_implementation.integration_class() {
        return Err(ProviderConfigurationError::ForeignIntegrationClass);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        CapabilityConstraints, CapabilityRequirement, CapabilitySet, FactProvenance, FactSource,
        FeatureSupport, Timestamp, ToolConcurrency, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    type TestResult2 = Result<EffectiveProviderConfigurationId, Box<dyn std::error::Error>>;

    struct StartCountingModel(AtomicUsize);

    impl LanguageModel for StartCountingModel {
        fn prepare_call(
            self: Arc<Self>,
            _plan: crate::ValidatedRequestPlan,
            _context: crate::ModelPreparationContext,
        ) -> Result<crate::PreparedModelCall, crate::ModelPreparationError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("registration tests never prepare a call")
        }
    }

    fn route() -> Result<ProviderRouteSelection, crate::IdentityError> {
        Ok(ProviderRouteSelection::new(
            ProtocolId::new("responses.subscription")?,
            RouteId::new("subscription.responses")?,
            ModelAdapterProfileId::new("responses-profile")?,
            ProviderProfileRevisionId::new("adapter-v1")?,
            ModelAdapterProfileId::new("oauth-profile")?,
            ProviderProfileRevisionId::new("auth-v1")?,
            ProviderAuthorityClassId::new("oauth-subscription")?,
            CredentialSelectionId::new("primary")?,
            ModelId::new("exact-model")?,
        ))
    }

    fn retention() -> Result<RetentionDisposition, crate::IdentityError> {
        Ok(RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new("retention-profile")?,
            revision: ProviderProfileRevisionId::new("retention-v1")?,
        })
    }

    fn behavior() -> Result<ProviderBehaviorConfiguration, crate::IdentityError> {
        Ok(ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::OmittedRouteContract,
            retention: retention()?,
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        })
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one matrix contrasts ordinary and workflow controls with every ceiling boundary"
    )]
    fn workflow_execution_controls_are_a_bounded_exact_refinement() -> TestResult {
        use crate::{
            AttemptId, ConversationView, GenerationLimits, ModelRequest, RequestMetadata, RunId,
            SessionWorkflowRootId, TurnId,
        };
        assert_eq!(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS.get(), 16_384);
        let selected = SelectedProviderConfigurationId::new(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"controls-selected"],
        )?)?;
        let effective = EffectiveProviderConfigurationId::new(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"controls-effective"],
        )?)?;
        let run = RunId::try_from(uuid::Uuid::from_u128(1))?;
        let attempt = AttemptId::try_from(uuid::Uuid::from_u128(2))?;
        let time = Timestamp::new(1, 0)?;
        let turn = RequestMetadata::for_turn(
            run,
            TurnId::try_from(uuid::Uuid::from_u128(3))?,
            attempt,
            time,
        );
        let workflow = RequestMetadata::for_session_workflow(
            run,
            SessionWorkflowRootId::new([1; 32])?,
            attempt,
            time,
        );
        let mut configured = behavior()?;
        configured.tool_choice = ToolChoice::Auto;
        for (ceiling, metadata, choice, requested, stops, allowed) in [
            (Some(8192), turn, ToolChoice::Auto, Some(8192), false, true),
            (Some(8192), turn, ToolChoice::None, Some(4096), false, false),
            // The compaction bound is the ceiling, capped by the turn bound.
            (
                Some(32768),
                workflow,
                ToolChoice::None,
                Some(16384),
                false,
                true,
            ),
            (
                Some(8192),
                workflow,
                ToolChoice::None,
                Some(8192),
                false,
                true,
            ),
            (
                Some(4096),
                workflow,
                ToolChoice::None,
                Some(4096),
                false,
                true,
            ),
            (
                Some(32768),
                workflow,
                ToolChoice::None,
                Some(4096),
                false,
                false,
            ),
            (
                Some(32768),
                workflow,
                ToolChoice::None,
                Some(16383),
                false,
                false,
            ),
            (
                Some(32768),
                workflow,
                ToolChoice::None,
                Some(16385),
                false,
                false,
            ),
            (
                Some(8192),
                workflow,
                ToolChoice::None,
                Some(4096),
                false,
                false,
            ),
            (None, workflow, ToolChoice::None, Some(16384), false, false),
            (
                Some(32768),
                workflow,
                ToolChoice::Auto,
                Some(16384),
                false,
                false,
            ),
            (
                Some(32768),
                workflow,
                ToolChoice::None,
                Some(16384),
                true,
                false,
            ),
            (None, turn, ToolChoice::Auto, None, false, true),
        ] {
            configured.output_bound = ceiling.and_then(NonZeroU32::new).map_or(
                OutputBoundDisposition::NoRequestBound,
                OutputBoundDisposition::Enforced,
            );
            let request =
                ModelRequest::builder(selected, effective, ConversationView::default(), metadata)
                    .tool_choice(choice)
                    .limits(GenerationLimits::new(
                        requested.and_then(NonZeroU32::new),
                        if stops {
                            vec!["stop".to_owned()]
                        } else {
                            Vec::new()
                        },
                    )?)
                    .build()?;
            assert_eq!(
                configured.matches_request_execution_controls(&request),
                allowed,
                "ceiling={ceiling:?} requested={requested:?} root={:?}",
                metadata.root()
            );
        }
        // A manual thinking budget keeps the summary above it, still under
        // the turn bound; a 4,096-token budget no longer equals the bound.
        configured.output_bound =
            OutputBoundDisposition::Enforced(NonZeroU32::new(32_768).ok_or("ceiling")?);
        for (budget, requested, allowed) in [
            (4_096, 12_288, true),
            (20_000, 28_192, true),
            (30_000, 32_768, true),
            (4_096, 16_384, false),
            (4_096, 4_096, false),
        ] {
            let reasoning = ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::TokenBudget(
                    NonZeroU32::new(budget).ok_or("budget")?,
                )),
                requirement: CapabilityRequirement::Required,
            };
            let request =
                ModelRequest::builder(selected, effective, ConversationView::default(), workflow)
                    .tool_choice(ToolChoice::None)
                    .reasoning(reasoning)
                    .limits(GenerationLimits::new(
                        NonZeroU32::new(requested),
                        Vec::new(),
                    )?)
                    .build()?;
            assert_eq!(
                configured.matches_request_execution_controls(&request),
                allowed,
                "budget={budget} requested={requested}"
            );
        }
        Ok(())
    }

    fn descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
        descriptor_at(1)
    }

    fn custom_descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
        let descriptor = descriptor()?;
        Ok(ResolvedModelDescriptor::new_custom(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            descriptor.reported_model().cloned(),
            descriptor.model_family().cloned(),
            descriptor.profile().clone(),
            descriptor.capabilities().clone(),
        ))
    }

    fn descriptor_at(observed_at: u64) -> Result<ResolvedModelDescriptor, crate::IdentityError> {
        descriptor_for(
            "subscription.responses",
            "exact-model",
            Some("exact-model"),
            observed_at,
        )
    }

    fn descriptor_for(
        route: &str,
        requested_model: &str,
        reported_model: Option<&str>,
        observed_at: u64,
    ) -> Result<ResolvedModelDescriptor, crate::IdentityError> {
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(observed_at, 0)?,
            Some(Timestamp::new(observed_at + 100, 0)?),
        );
        let features = [
            Capability::Reasoning,
            Capability::Tools,
            Capability::ParallelTools,
            Capability::HostedCodeExecution,
            Capability::HostedProgrammaticToolCalling,
            Capability::StructuredOutput,
            Capability::Caching,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect();
        Ok(ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("responses.subscription")?,
            RouteId::new(route)?,
            ModelId::new(requested_model)?,
            reported_model.map(ModelId::new).transpose()?,
            Some(ModelFamilyId::new("responses")?),
            ModelAdapterProfileId::new("responses-profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        ))
    }

    fn authority(
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<RuntimeModelAuthority, Box<dyn std::error::Error>> {
        let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
        let credentials = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credentials"])?;
        Ok(RuntimeModelAuthority::new(
            descriptor,
            account,
            credentials,
            1,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(2, 0)?),
        )?)
    }

    fn stable_authority(
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<ResolvedProviderAuthorityBinding, Box<dyn std::error::Error>> {
        Ok(ResolvedProviderAuthorityBinding::new(
            descriptor,
            &authority(descriptor)?,
            ProviderSemanticEvidence::new(1, b"fixture-semantic-evidence-v1".to_vec())?,
        )?)
    }

    fn family_policy(bytes: &[u8]) -> Result<FamilyPolicyEnvelope, Box<dyn std::error::Error>> {
        Ok(FamilyPolicyEnvelope::new(
            ModelFamilyId::new("responses")?,
            1,
            1,
            1,
            bytes.to_vec(),
        )?)
    }

    fn implementation() -> Result<ModelImplementationIdentity, Box<dyn std::error::Error>> {
        Ok(ModelImplementationIdentity::from_parts(
            crate::IntegrationClass::Standard,
            NamespaceId::new("plakous.test.scripted-model")?,
            1,
            NamespaceId::new("plakous.release")?,
        )?)
    }

    fn compiled(
        behavior: ProviderBehaviorConfiguration,
        policy: &[u8],
    ) -> Result<CompiledProviderConfiguration, Box<dyn std::error::Error>> {
        let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
        let descriptor = descriptor()?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            implementation()?,
            stable_authority(&descriptor)?,
            behavior,
            family_policy(policy)?,
        )?;
        Ok(CompiledProviderConfiguration::new(selected, effective)?)
    }

    #[derive(Clone, Copy)]
    struct EffectiveIdentityCase {
        route: &'static str,
        requested_model: &'static str,
        reported_model: &'static str,
        account: &'static [u8],
        credential_authority: &'static [u8],
        catalog_source_revision: u32,
        catalog_facts: &'static [u8],
        policy_schema_revision: u32,
        policy_compiler_revision: u32,
        policy_dialect_revision: u32,
        policy_facts: &'static [u8],
    }

    impl Default for EffectiveIdentityCase {
        fn default() -> Self {
            Self {
                route: "subscription.responses",
                requested_model: "exact-model",
                reported_model: "exact-model",
                account: b"account",
                credential_authority: b"credentials",
                catalog_source_revision: 1,
                catalog_facts: b"fixture-semantic-evidence-v1",
                policy_schema_revision: 1,
                policy_compiler_revision: 1,
                policy_dialect_revision: 1,
                policy_facts: b"policy-one",
            }
        }
    }

    fn compile_identity_case(
        case: EffectiveIdentityCase,
    ) -> Result<CompiledProviderConfiguration, Box<dyn std::error::Error>> {
        let behavior = behavior()?;
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                ProtocolId::new("responses.subscription")?,
                RouteId::new(case.route)?,
                ModelAdapterProfileId::new("responses-profile")?,
                ProviderProfileRevisionId::new("adapter-v1")?,
                ModelAdapterProfileId::new("oauth-profile")?,
                ProviderProfileRevisionId::new("auth-v1")?,
                ProviderAuthorityClassId::new("oauth-subscription")?,
                CredentialSelectionId::new("primary")?,
                ModelId::new(case.requested_model)?,
            ),
            behavior.clone(),
        )?;
        let descriptor = descriptor_for(
            case.route,
            case.requested_model,
            Some(case.reported_model),
            1,
        )?;
        let runtime = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[case.account])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[case.credential_authority])?,
            1,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(2, 0)?),
        )?;
        let stable = ResolvedProviderAuthorityBinding::new(
            &descriptor,
            &runtime,
            ProviderSemanticEvidence::new(
                case.catalog_source_revision,
                case.catalog_facts.to_vec(),
            )?,
        )?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor,
            implementation()?,
            stable,
            behavior,
            FamilyPolicyEnvelope::new(
                ModelFamilyId::new("responses")?,
                case.policy_schema_revision,
                case.policy_compiler_revision,
                case.policy_dialect_revision,
                case.policy_facts.to_vec(),
            )?,
        )?;
        Ok(CompiledProviderConfiguration::new(selected, effective)?)
    }

    #[test]
    fn configuration_roundtrip_is_reconstruction_complete_and_deterministic() -> TestResult {
        let first = compiled(behavior()?, b"policy-one")?;
        let second = compiled(behavior()?, b"policy-one")?;
        assert_eq!(first, second);
        let bytes = serde_json::to_vec(&first)?;
        let decoded: CompiledProviderConfiguration = serde_json::from_slice(&bytes)?;
        assert_eq!(decoded, first);
        assert_eq!(
            decoded.selected().route().requested_model().as_str(),
            "exact-model"
        );
        assert_eq!(
            decoded.effective().family_policy().canonical_bytes(),
            b"policy-one"
        );
        Ok(())
    }

    fn configured_tool(
        description: &str,
        execution: ToolExecutionDisposition,
    ) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
        Ok(ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1.0.0",
                description,
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            execution,
            ToolCallerDisposition::DirectOnly,
        ))
    }

    /// A bound the product derives is identity-bearing like every other field,
    /// and the two dispositions are distinct even at the same figure.
    ///
    /// It has to be. The stored identity is recomputed from the stored
    /// behaviour on every decode and compared to the stored digest, so any
    /// convention that dropped the figure would make every store sealed under
    /// the old one undecodable. Reopen therefore tolerates the movement by
    /// substitution, not by digesting less.
    #[test]
    fn a_derived_bound_is_identity_bearing_and_distinct_from_a_chosen_one() -> TestResult {
        let bound = NonZeroU32::new(32_768).ok_or("bound")?;
        let far = NonZeroU32::new(200_000).ok_or("far")?;
        let identity = |disposition| -> TestResult2 {
            let mut value = behavior()?;
            value.output_bound = disposition;
            Ok(compiled(value, b"policy-one")?.effective_identity())
        };
        assert_ne!(
            identity(OutputBoundDisposition::DerivedFromRoute(bound))?,
            identity(OutputBoundDisposition::DerivedFromRoute(far))?,
            "the figure is digested, so a moved figure is a moved identity"
        );
        assert_ne!(
            identity(OutputBoundDisposition::Enforced(bound))?,
            identity(OutputBoundDisposition::DerivedFromRoute(bound))?,
            "the disposition is digested, so the two spellings differ"
        );
        assert!(OutputBoundDisposition::DerivedFromRoute(bound).is_derived_from_route());
        assert!(!OutputBoundDisposition::Enforced(bound).is_derived_from_route());
        assert_eq!(
            OutputBoundDisposition::DerivedFromRoute(bound).bound(),
            Some(bound)
        );
        assert_eq!(OutputBoundDisposition::NoRequestBound.bound(), None);
        Ok(())
    }

    /// Reopen asks whether everything but the derived figure still matches.
    ///
    /// Three session shapes have to reopen on a route that derives its bound.
    /// One created under a bound the product chose, before the route derived
    /// anything; one created under the same derived figure the route still
    /// attests; and one created under a derived figure that has since moved.
    /// A configuration that differs in anything else reopens under none of
    /// them, however the bound is substituted.
    #[test]
    fn substituting_the_persisted_bound_reopens_the_same_route_and_no_other() -> TestResult {
        let chosen_then = NonZeroU32::new(4_096).ok_or("chosen")?;
        let derived_now = NonZeroU32::new(32_768).ok_or("derived")?;

        let mut behavior_now = behavior()?;
        behavior_now.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
        let now = compiled(behavior_now, b"policy-one")?;
        let live = (now.selected_identity(), now.effective_identity());

        let persisted = |disposition| -> Result<
            (
                SelectedProviderConfigurationId,
                EffectiveProviderConfigurationId,
            ),
            Box<dyn std::error::Error>,
        > {
            let mut value = behavior()?;
            value.output_bound = disposition;
            let compiled = compiled(value, b"policy-one")?;
            Ok((compiled.selected_identity(), compiled.effective_identity()))
        };

        // Created under a chosen bound, before this route derived anything.
        let chosen = persisted(OutputBoundDisposition::Enforced(chosen_then))?;
        assert_ne!(chosen, live);
        assert_eq!(now.reopen_identities(Some(chosen_then), chosen)?, chosen);

        // Created under the figure the route still attests: nothing to move.
        let unmoved = persisted(OutputBoundDisposition::DerivedFromRoute(derived_now))?;
        assert_eq!(unmoved, live);
        assert_eq!(now.reopen_identities(Some(derived_now), unmoved)?, live);

        // Created under a derived figure the route has since moved.
        let moved_from = NonZeroU32::new(8_192).ok_or("moved")?;
        let moved = persisted(OutputBoundDisposition::DerivedFromRoute(moved_from))?;
        assert_ne!(moved, live);
        assert_eq!(now.reopen_identities(Some(moved_from), moved)?, moved);

        // A configuration that differs in something else never agrees, whatever
        // bound is substituted. `local_durability` stands for any of the fields
        // the identity covers.
        let mut other_behavior = behavior()?;
        other_behavior.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
        other_behavior.local_durability = LocalDurabilityDisposition::DurableRequired;
        let other = compiled(other_behavior, b"policy-one")?;
        assert_ne!(other.reopen_identities(Some(chosen_then), chosen)?, chosen);
        assert_ne!(other.reopen_identities(Some(moved_from), moved)?, moved);

        // And a different family policy, which stands for a different route.
        let mut foreign_behavior = behavior()?;
        foreign_behavior.output_bound = OutputBoundDisposition::DerivedFromRoute(derived_now);
        let foreign = compiled(foreign_behavior, b"policy-two")?;
        assert_ne!(
            foreign.reopen_identities(Some(chosen_then), chosen)?,
            chosen
        );

        // A route whose bound the product chose keeps the exact comparison, so
        // a changed bound there is still drift.
        let mut chosen_live = behavior()?;
        chosen_live.output_bound = OutputBoundDisposition::Enforced(derived_now);
        let chosen_live = compiled(chosen_live, b"policy-one")?;
        assert_ne!(
            chosen_live.reopen_identities(Some(chosen_then), chosen)?,
            chosen
        );
        Ok(())
    }

    #[test]
    fn every_behavior_field_changes_selected_and_effective_identity() -> TestResult {
        let baseline = compiled(behavior()?, b"policy-one")?;
        let mut variants: Vec<(&str, ProviderBehaviorConfiguration)> = Vec::new();
        let mut value = behavior()?;
        value.processing_tier =
            ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?);
        variants.push(("processing_tier", value));
        let mut value = behavior()?;
        value.reasoning_effort = ReasoningEffortDisposition::Enabled(ReasoningEffort::Low);
        variants.push(("reasoning_effort", value));
        let mut value = behavior()?;
        value.reasoning_summary = ReasoningSummaryDisposition::Concise;
        variants.push(("reasoning_summary", value));
        let mut value = behavior()?;
        value.reasoning_include = ReasoningIncludeDisposition::EncryptedContent;
        variants.push(("reasoning_include", value));
        let mut value = behavior()?;
        value.replay = ReplayDisposition::DropUnreplayableReasoning;
        variants.push(("replay", value));
        let mut value = behavior()?;
        value.cache = CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        };
        variants.push(("cache", value));
        let mut value = behavior()?;
        value.tools = vec![configured_tool(
            "lookup definition one",
            ToolExecutionDisposition::Local,
        )?];
        variants.push(("tool_definition", value));
        let mut value = behavior()?;
        value.tools = vec![configured_tool(
            "lookup definition",
            ToolExecutionDisposition::ProviderHosted,
        )?];
        variants.push(("tool_owner", value));
        let mut value = behavior()?;
        value.tools = vec![configured_tool(
            "lookup definition",
            ToolExecutionDisposition::Local,
        )?];
        value.tool_choice = ToolChoice::Specific(ToolName::new("lookup")?);
        variants.push(("tool_choice", value));
        let mut value = behavior()?;
        value.tools = vec![configured_tool(
            "lookup definition",
            ToolExecutionDisposition::Local,
        )?];
        value.parallel_tools = ParallelToolDisposition::Enabled;
        variants.push(("parallel_tools", value));
        let mut value = behavior()?;
        value.output = OutputPolicy::new(
            OutputFormat::structured(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"answer": {"type": "string"}},
                "required": ["answer"],
                "additionalProperties": false
            }))?,
            CapabilityRequirement::Required,
        )?;
        variants.push(("output_format", value));
        let mut value = behavior()?;
        value.output_bound = OutputBoundDisposition::Enforced(NonZeroU32::MIN);
        variants.push(("output_bound", value));
        let mut value = behavior()?;
        value.storage = ProviderStorageDisposition::DisabledOnWire;
        variants.push(("storage", value));
        let mut value = behavior()?;
        value.retention = RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new("retention-profile")?,
            revision: ProviderProfileRevisionId::new("retention-v2")?,
        };
        variants.push(("retention", value));
        let mut value = behavior()?;
        value.response_state = ResponseStateDisposition::ProviderStored;
        variants.push(("response_state", value));
        let mut value = behavior()?;
        value.context_evidence_requirement = ContextAdmissionRequirement::ReleaseVerifiedRequired;
        variants.push(("context_evidence_requirement", value));
        let mut value = behavior()?;
        value.local_durability = LocalDurabilityDisposition::DurableRequired;
        variants.push(("local_durability", value));
        for (field, variant) in variants {
            let changed = compiled(variant, b"policy-one")?;
            assert_ne!(
                changed.selected_identity(),
                baseline.selected_identity(),
                "{field}"
            );
            assert_ne!(
                changed.effective_identity(),
                baseline.effective_identity(),
                "{field}"
            );
        }
        Ok(())
    }

    #[test]
    fn every_resolved_authority_and_policy_field_changes_exact_effective_identity() -> TestResult {
        let baseline_case = EffectiveIdentityCase::default();
        let baseline = compile_identity_case(baseline_case)?;
        let variants = [
            (
                "route",
                EffectiveIdentityCase {
                    route: "subscription.responses.v2",
                    ..baseline_case
                },
            ),
            (
                "requested_model",
                EffectiveIdentityCase {
                    requested_model: "requested-model-v2",
                    ..baseline_case
                },
            ),
            (
                "effective_model",
                EffectiveIdentityCase {
                    reported_model: "effective-model-v2",
                    ..baseline_case
                },
            ),
            (
                "account",
                EffectiveIdentityCase {
                    account: b"account-v2",
                    ..baseline_case
                },
            ),
            (
                "credential_authority",
                EffectiveIdentityCase {
                    credential_authority: b"credentials-v2",
                    ..baseline_case
                },
            ),
            (
                "catalog_modality_fact",
                EffectiveIdentityCase {
                    catalog_facts: b"catalog-input-modalities-text-image",
                    ..baseline_case
                },
            ),
            (
                "catalog_source_revision",
                EffectiveIdentityCase {
                    catalog_source_revision: 2,
                    ..baseline_case
                },
            ),
            (
                "policy_fact",
                EffectiveIdentityCase {
                    policy_facts: b"policy-two",
                    ..baseline_case
                },
            ),
            (
                "policy_schema_revision",
                EffectiveIdentityCase {
                    policy_schema_revision: 2,
                    ..baseline_case
                },
            ),
            (
                "policy_compiler_revision",
                EffectiveIdentityCase {
                    policy_compiler_revision: 2,
                    ..baseline_case
                },
            ),
            (
                "policy_dialect_revision",
                EffectiveIdentityCase {
                    policy_dialect_revision: 2,
                    ..baseline_case
                },
            ),
        ];
        for (field, variant) in variants {
            let changed = compile_identity_case(variant)?;
            assert_ne!(
                changed.effective_identity(),
                baseline.effective_identity(),
                "{field}"
            );
        }
        Ok(())
    }

    #[test]
    fn selected_and_effective_behavior_cannot_disagree() -> TestResult {
        let selected_behavior = behavior()?;
        let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
        let descriptor = descriptor()?;
        let mut different = selected_behavior;
        different.local_durability = LocalDurabilityDisposition::DurableRequired;
        assert_eq!(
            EffectiveProviderConfiguration::compile(
                &selected,
                descriptor.clone(),
                implementation()?,
                stable_authority(&descriptor)?,
                different,
                family_policy(b"policy-one")?,
            ),
            Err(ProviderConfigurationError::UnsupportedBehavior)
        );
        Ok(())
    }

    #[test]
    fn tool_presentation_mode_rotates_selected_and_effective_identity() -> TestResult {
        let direct = behavior()?;
        let selected_direct = SelectedProviderConfiguration::new(route()?, direct.clone())?;
        let effective_direct = compile_identity_case(EffectiveIdentityCase::default())?;

        let mut composition_only = direct;
        composition_only.tool_mode = ToolPresentationMode::LocalCompositionOnly;
        let selected_composition =
            SelectedProviderConfiguration::new(route()?, composition_only.clone())?;
        let descriptor = descriptor()?;
        let effective_composition = EffectiveProviderConfiguration::compile(
            &selected_composition,
            descriptor.clone(),
            implementation()?,
            stable_authority(&descriptor)?,
            composition_only,
            family_policy(b"policy-one")?,
        )?;

        assert_ne!(selected_direct.identity(), selected_composition.identity());
        assert_ne!(
            effective_direct.effective_identity(),
            effective_composition.identity()
        );
        let round_trip: SelectedProviderConfiguration =
            serde_json::from_slice(&serde_json::to_vec(&selected_composition)?)?;
        assert_eq!(
            round_trip.behavior().tool_mode,
            ToolPresentationMode::LocalCompositionOnly
        );
        Ok(())
    }

    #[test]
    fn hosted_selection_and_per_tool_caller_disposition_are_exact_configuration_facts() -> TestResult
    {
        let direct = behavior()?;
        let direct_selected = SelectedProviderConfiguration::new(route()?, direct.clone())?;

        let mut programmatic = behavior()?;
        programmatic.tools = vec![ConfiguredTool::new(
            configured_tool("lookup", ToolExecutionDisposition::Local)?
                .definition()
                .clone(),
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::ProviderProgrammaticOnly,
        )];
        programmatic.hosted_tools = vec![HostedTool::ProgrammaticComposition];
        programmatic.tool_choice = ToolChoice::Auto;
        let programmatic_selected =
            SelectedProviderConfiguration::new(route()?, programmatic.clone())?;
        assert_ne!(direct_selected.identity(), programmatic_selected.identity());

        let mut both = programmatic.clone();
        both.tools[0] = ConfiguredTool::new(
            both.tools[0].definition().clone(),
            both.tools[0].execution(),
            ToolCallerDisposition::DirectAndProviderProgrammatic,
        );
        let both_selected = SelectedProviderConfiguration::new(route()?, both)?;
        assert_ne!(programmatic_selected.identity(), both_selected.identity());

        let descriptor = descriptor()?;
        let effective = EffectiveProviderConfiguration::compile(
            &programmatic_selected,
            descriptor.clone(),
            implementation()?,
            stable_authority(&descriptor)?,
            programmatic,
            family_policy(b"hosted-policy")?,
        )?;
        assert_eq!(
            effective.behavior().hosted_tools,
            vec![HostedTool::ProgrammaticComposition]
        );
        let round_trip: EffectiveProviderConfiguration =
            serde_json::from_slice(&serde_json::to_vec(&effective)?)?;
        assert_eq!(round_trip, effective);
        Ok(())
    }

    #[test]
    fn tool_owner_projection_matrix_is_exact_and_does_not_coerce_ownership() -> TestResult {
        let definition = configured_tool("lookup", ToolExecutionDisposition::Local)?
            .definition()
            .clone();
        let direct = ConfiguredTool::new(
            definition.clone(),
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        );
        let programmatic = ConfiguredTool::new(
            definition.clone(),
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectAndProviderProgrammatic,
        );
        let mcp = ToolExecutionOwner::Mcp {
            server: McpServerId::new("plak-mcp")?,
        };
        assert_eq!(direct.validate_owner_projection(&mcp), Ok(()));
        assert_eq!(
            programmatic.validate_owner_projection(&ToolExecutionOwner::Local),
            Ok(())
        );
        assert_eq!(programmatic.validate_owner_projection(&mcp), Ok(()));

        let hosted = ConfiguredTool::new(
            definition,
            ToolExecutionDisposition::ProviderHosted,
            ToolCallerDisposition::DirectOnly,
        );
        let family_owner = ToolExecutionOwner::ProviderHosted {
            provider_kind: NamespaceId::new("family.hosted")?,
        };
        assert_eq!(hosted.validate_owner_projection(&family_owner), Ok(()));
        assert_eq!(
            hosted.validate_owner_projection(&ToolExecutionOwner::Local),
            Err(ToolOwnerProjectionError::OwnerMismatch)
        );
        assert_eq!(
            direct.validate_owner_projection(&family_owner),
            Err(ToolOwnerProjectionError::OwnerMismatch)
        );
        Ok(())
    }

    #[test]
    fn an_mcp_owner_is_absent_from_every_other_tool_and_exact_when_present() -> TestResult {
        let local = configured_tool("lookup", ToolExecutionDisposition::Local)?;
        let encoded = serde_json::to_value(&local)?;
        let keys = encoded
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(keys, ["caller", "definition", "execution"]);
        assert_eq!(local.local_owner(), Some(ToolExecutionOwner::Local));

        let mut plain_behavior = behavior()?;
        plain_behavior.tools = vec![local.clone()];
        let plain = compiled(plain_behavior.clone(), b"policy-one")?;
        let plain_bytes = serde_json::to_vec(&plain)?;
        assert!(!String::from_utf8(plain_bytes.clone())?.contains("mcp_server"));
        let decoded: CompiledProviderConfiguration = serde_json::from_slice(&plain_bytes)?;
        assert_eq!(serde_json::to_vec(&decoded)?, plain_bytes);
        assert_eq!(decoded.effective().identity(), plain.effective().identity());

        let server = McpServerId::new("github")?;
        let owned = local.clone().with_mcp_owner(server.clone())?;
        let mcp_owner = ToolExecutionOwner::Mcp {
            server: server.clone(),
        };
        assert_eq!(owned.local_owner(), Some(mcp_owner.clone()));
        assert_eq!(owned.validate_owner_projection(&mcp_owner), Ok(()));
        for wrong in [
            ToolExecutionOwner::Local,
            ToolExecutionOwner::Mcp {
                server: McpServerId::new("linear")?,
            },
        ] {
            assert_eq!(
                owned.validate_owner_projection(&wrong),
                Err(ToolOwnerProjectionError::OwnerMismatch)
            );
        }

        let mut owned_behavior = plain_behavior;
        owned_behavior.tools = vec![owned];
        let mcp = compiled(owned_behavior, b"policy-one")?;
        assert_ne!(mcp.selected().identity(), plain.selected().identity());
        assert_ne!(mcp.effective().identity(), plain.effective().identity());
        let mcp_bytes = serde_json::to_vec(&mcp)?;
        assert!(String::from_utf8(mcp_bytes.clone())?.contains("\"mcp_server\":\"github\""));
        let decoded: CompiledProviderConfiguration = serde_json::from_slice(&mcp_bytes)?;
        assert_eq!(decoded, mcp);

        let hosted = configured_tool("lookup", ToolExecutionDisposition::ProviderHosted)?;
        assert_eq!(hosted.local_owner(), None);
        assert_eq!(
            hosted.with_mcp_owner(server).err(),
            Some(ToolOwnerProjectionError::OwnerMismatch)
        );
        Ok(())
    }

    #[test]
    fn hosted_configuration_fails_closed_on_shape_capability_and_compaction() -> TestResult {
        let definition = configured_tool("lookup", ToolExecutionDisposition::Local)?
            .definition()
            .clone();
        let mut missing_selection = behavior()?;
        missing_selection.tools = vec![ConfiguredTool::new(
            definition.clone(),
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::ProviderProgrammaticOnly,
        )];
        missing_selection.tool_choice = ToolChoice::Auto;
        assert!(SelectedProviderConfiguration::new(route()?, missing_selection).is_err());

        let mut no_eligible_tool = behavior()?;
        no_eligible_tool.tools = vec![ConfiguredTool::new(
            definition,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        )];
        no_eligible_tool.hosted_tools = vec![HostedTool::ProgrammaticComposition];
        no_eligible_tool.tool_choice = ToolChoice::Auto;
        assert!(SelectedProviderConfiguration::new(route()?, no_eligible_tool).is_err());

        let mut interpreter = behavior()?;
        interpreter.hosted_tools = vec![HostedTool::CodeInterpreter];
        let selected = SelectedProviderConfiguration::new(route()?, interpreter.clone())?;
        let supported = descriptor()?;
        let mut features = supported.capabilities().features().clone();
        features.remove(&Capability::HostedCodeExecution);
        let unsupported = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            supported.protocol().clone(),
            supported.route().clone(),
            supported.requested_model().clone(),
            supported.reported_model().cloned(),
            supported.model_family().cloned(),
            supported.profile().clone(),
            CapabilitySet::new(features, BTreeMap::new()),
        );
        assert!(matches!(
            EffectiveProviderConfiguration::compile(
                &selected,
                unsupported.clone(),
                implementation()?,
                stable_authority(&unsupported)?,
                interpreter,
                family_policy(b"hosted-policy")?,
            ),
            Err(ProviderConfigurationError::UnsupportedBehavior)
        ));

        let mut hosted = behavior()?;
        hosted.hosted_tools = vec![HostedTool::CodeInterpreter];
        let compacted = compiled(hosted, b"hosted-policy")?.semantic_compaction()?;
        assert_eq!(
            compacted.selected().behavior().tools,
            [] as [ConfiguredTool; 0]
        );
        assert_eq!(
            compacted.selected().behavior().hosted_tools,
            [] as [HostedTool; 0]
        );
        Ok(())
    }

    #[test]
    fn semantic_compaction_preserves_exact_processing_tier() -> TestResult {
        let mut priority = behavior()?;
        priority.processing_tier =
            ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?);

        let compacted = compiled(priority, b"priority-compaction")?.semantic_compaction()?;

        assert_eq!(
            compacted.selected().behavior().processing_tier,
            ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?)
        );
        assert_eq!(
            compacted.effective().behavior().processing_tier,
            ProcessingTierDisposition::ProviderNamed(ProviderServiceTierId::new("priority")?)
        );
        Ok(())
    }

    #[test]
    fn missing_tampered_and_cross_bound_values_fail_closed() -> TestResult {
        let value = compiled(behavior()?, b"policy-one")?;
        let mut missing = serde_json::to_value(&value)?;
        missing["selected"]
            .as_object_mut()
            .ok_or("selected fixture must be an object")?
            .remove("behavior");
        assert!(serde_json::from_value::<CompiledProviderConfiguration>(missing).is_err());

        let mut tampered = serde_json::to_value(&value)?;
        tampered["effective"]["family_policy"]["canonical_bytes"] = serde_json::json!([9]);
        assert!(serde_json::from_value::<CompiledProviderConfiguration>(tampered).is_err());

        let other = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                ProtocolId::new("responses.subscription")?,
                RouteId::new("subscription.responses")?,
                ModelAdapterProfileId::new("responses-profile")?,
                ProviderProfileRevisionId::new("adapter-v2")?,
                ModelAdapterProfileId::new("oauth-profile")?,
                ProviderProfileRevisionId::new("auth-v1")?,
                ProviderAuthorityClassId::new("oauth-subscription")?,
                CredentialSelectionId::new("primary")?,
                ModelId::new("exact-model")?,
            ),
            behavior()?,
        )?;
        assert_eq!(
            CompiledProviderConfiguration::new(other, value.effective().clone()),
            Err(ProviderConfigurationError::ForeignSelection)
        );
        Ok(())
    }

    #[test]
    fn generic_capability_mismatch_is_rejected() -> TestResult {
        let selected_behavior = ProviderBehaviorConfiguration {
            tool_mode: ToolPresentationMode::Direct,
            cache: CachePolicy::Enabled {
                requirement: CapabilityRequirement::Required,
            },
            hosted_tools: Vec::new(),
            ..behavior()?
        };
        let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
        let descriptor = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("responses.subscription")?,
            RouteId::new("subscription.responses")?,
            ModelId::new("exact-model")?,
            Some(ModelId::new("exact-model")?),
            Some(ModelFamilyId::new("responses")?),
            ModelAdapterProfileId::new("responses-profile")?,
            CapabilitySet::default(),
        );
        assert_eq!(
            EffectiveProviderConfiguration::compile(
                &selected,
                descriptor.clone(),
                implementation()?,
                stable_authority(&descriptor)?,
                selected_behavior,
                family_policy(b"policy")?,
            ),
            Err(ProviderConfigurationError::UnsupportedBehavior)
        );
        Ok(())
    }

    #[test]
    fn refresh_only_authority_changes_do_not_rotate_effective_semantics() -> TestResult {
        let selected_behavior = behavior()?;
        let selected = SelectedProviderConfiguration::new(route()?, selected_behavior.clone())?;
        let first_descriptor = descriptor_at(1)?;
        let second_descriptor = descriptor_at(10)?;
        assert_ne!(
            first_descriptor.binding_digest()?,
            second_descriptor.binding_digest()?
        );
        assert_eq!(
            first_descriptor.semantic_binding_digest()?,
            second_descriptor.semantic_binding_digest()?
        );
        let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
        let credentials = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credentials"])?;
        let first_runtime = RuntimeModelAuthority::new(
            &first_descriptor,
            account,
            credentials,
            1,
            Timestamp::new(1, 0)?,
            Some(Timestamp::new(101, 0)?),
        )?;
        let second_runtime = RuntimeModelAuthority::new(
            &second_descriptor,
            account,
            credentials,
            2,
            Timestamp::new(10, 0)?,
            Some(Timestamp::new(110, 0)?),
        )?;
        let semantic = ProviderSemanticEvidence::new(1, b"same-semantic-facts".to_vec())?;
        let first_authority = ResolvedProviderAuthorityBinding::new(
            &first_descriptor,
            &first_runtime,
            semantic.clone(),
        )?;
        let second_authority =
            ResolvedProviderAuthorityBinding::new(&second_descriptor, &second_runtime, semantic)?;
        let first = EffectiveProviderConfiguration::compile(
            &selected,
            first_descriptor,
            implementation()?,
            first_authority,
            selected_behavior.clone(),
            family_policy(b"policy")?,
        )?;
        let second = EffectiveProviderConfiguration::compile(
            &selected,
            second_descriptor,
            implementation()?,
            second_authority,
            selected_behavior.clone(),
            family_policy(b"policy")?,
        )?;
        assert_eq!(first.identity(), second.identity());

        let changed_authority = ResolvedProviderAuthorityBinding::new(
            first.descriptor(),
            &first_runtime,
            ProviderSemanticEvidence::new(1, b"changed-semantic-facts".to_vec())?,
        )?;
        let changed = EffectiveProviderConfiguration::compile(
            &selected,
            first.descriptor().clone(),
            implementation()?,
            changed_authority,
            selected_behavior,
            family_policy(b"policy")?,
        )?;
        assert_ne!(first.identity(), changed.identity());
        Ok(())
    }

    #[test]
    fn custom_registration_owns_descriptor_executable_and_implementation() -> TestResult {
        let behavior = behavior()?;
        let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
        let descriptor = custom_descriptor()?;
        let runtime = authority(&descriptor)?;
        let implementation = ModelImplementationIdentity::custom(
            NamespaceId::new("consumer.custom-model")?,
            3,
            NamespaceId::new("consumer.fixture")?,
        )?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            implementation.clone(),
            stable_authority(&descriptor)?,
            behavior,
            family_policy(b"custom-policy")?,
        )?;
        let configuration = CompiledProviderConfiguration::new(selected, effective)?;
        let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
        let configured = ConfiguredLanguageModel::custom(starts.clone(), configuration, runtime)?;
        assert_eq!(configured.registered().descriptor(), &descriptor);
        assert_eq!(configured.registered().implementation(), &implementation);
        assert_eq!(starts.0.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn custom_registration_rejects_standard_configuration_before_start() -> TestResult {
        let configuration = compiled(behavior()?, b"standard-policy")?;
        let descriptor = configuration.effective().descriptor().clone();
        let runtime = authority(&descriptor)?;
        let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
        let error = ConfiguredLanguageModel::custom(starts.clone(), configuration, runtime)
            .err()
            .ok_or("standard configuration was accepted by the custom lane")?;
        assert_eq!(error, ProviderConfigurationError::ForeignIntegrationClass);
        assert_eq!(starts.0.load(Ordering::SeqCst), 0);
        Ok(())
    }
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one matrix compares deserialization, compilation and both registration authorities for every evidence class"
    )]
    fn evidence_compatibility_matrix_keeps_registration_authority_separate() -> TestResult {
        use crate::{
            CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
            CountCertificateKind, EvidenceFreshness, IntegrationClass, MeasurementCertificate,
            SafeLimit, StandardIntegrationAuthority,
        };
        let standard_authority = StandardIntegrationAuthority::release_owned();
        let starts = Arc::new(StartCountingModel(AtomicUsize::new(0)));
        let standard_descriptor = descriptor()?;
        let binding = authority(&standard_descriptor)?.authority_class_binding()?;
        let receipt = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[b"matrix-context-receipt"],
        )?;
        let freshness =
            EvidenceFreshness::new(Timestamp::new(1, 0)?, Some(Timestamp::new(100, 0)?))?;
        for integration in [IntegrationClass::Standard, IntegrationClass::Custom] {
            for evidence in 0..3 {
                let (capacity, source, count) = match evidence {
                    0 => (
                        CapacityCertificate::new(
                            &standard_authority,
                            "matrix-capacity",
                            "v1",
                            ContextEvidenceSource::ReleaseCertificate,
                            CapacityRule::SharedWindow {
                                total: SafeLimit::Exact(100),
                            },
                            None,
                            freshness,
                        )?,
                        ContextEvidenceSource::ReleaseCertificate,
                        CountCertificateKind::ExactTokenizer,
                    ),
                    1 => (
                        CapacityCertificate::reference_client_envelope(
                            &standard_authority,
                            "matrix-capacity",
                            "v1",
                            CapacityRule::ProviderReservedInput {
                                reported_context_window: None,
                                maximum_projected_input: 100,
                                auto_compact_input: None,
                            },
                            binding,
                            freshness,
                            receipt,
                            "v1",
                        )?,
                        ContextEvidenceSource::AuthenticatedDiscovery,
                        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                            catalog_receipt: receipt,
                        },
                    ),
                    _ => (
                        CapacityCertificate::custom_envelope(
                            "matrix-capacity",
                            "v1",
                            CapacityRule::ProviderReservedInput {
                                reported_context_window: None,
                                maximum_projected_input: 100,
                                auto_compact_input: None,
                            },
                            binding,
                            freshness,
                            receipt,
                            "v1",
                        )?,
                        ContextEvidenceSource::CallerDeclaration,
                        CountCertificateKind::CustomEnvelopeUpperBound {
                            declaration: receipt,
                        },
                    ),
                };
                let measurement = if evidence == 2 {
                    MeasurementCertificate::new(
                        "matrix-count",
                        "v1",
                        "matrix-count",
                        "v1",
                        source,
                        count,
                        Some(binding),
                        freshness,
                    )?
                } else {
                    MeasurementCertificate::standard(
                        &standard_authority,
                        "matrix-count",
                        "v1",
                        "matrix-count",
                        "v1",
                        source,
                        count,
                        (evidence != 0).then_some(binding),
                        freshness,
                    )?
                };
                let context = ContextContract::new(
                    ProtocolId::new("responses-family")?,
                    "v1",
                    "v1",
                    "v1",
                    capacity,
                    vec![measurement],
                )?;
                let descriptor = if integration == IntegrationClass::Standard {
                    standard_descriptor.clone()
                } else {
                    custom_descriptor()?
                }
                .with_context_contract(context);
                let allowed = integration == IntegrationClass::Standard || evidence == 2;
                let wire = serde_json::to_vec(&descriptor)?;
                let decoded = serde_json::from_slice::<ResolvedModelDescriptor>(&wire);
                assert_eq!(
                    decoded.is_ok(),
                    allowed,
                    "descriptor {integration:?} evidence {evidence}"
                );
                let selected_implementation = if integration == IntegrationClass::Standard {
                    implementation()?
                } else {
                    ModelImplementationIdentity::custom(
                        NamespaceId::new("matrix.custom")?,
                        1,
                        NamespaceId::new("matrix.caller")?,
                    )?
                };
                let registered = if integration == IntegrationClass::Standard {
                    RegisteredLanguageModel::standard(
                        &standard_authority,
                        descriptor.clone(),
                        selected_implementation.clone(),
                        starts.clone(),
                    )
                } else {
                    RegisteredLanguageModel::custom(
                        descriptor.clone(),
                        selected_implementation.clone(),
                        starts.clone(),
                    )
                };
                assert_eq!(
                    registered.is_ok(),
                    allowed,
                    "registration {integration:?} evidence {evidence}"
                );
                let behavior = behavior()?;
                let selected = SelectedProviderConfiguration::new(route()?, behavior.clone())?;
                let effective = EffectiveProviderConfiguration::compile(
                    &selected,
                    descriptor.clone(),
                    selected_implementation.clone(),
                    stable_authority(&descriptor)?,
                    behavior,
                    family_policy(b"matrix-policy")?,
                );
                assert_eq!(
                    effective.is_ok(),
                    allowed,
                    "compilation {integration:?} evidence {evidence}"
                );
                if integration == IntegrationClass::Standard {
                    assert!(
                        RegisteredLanguageModel::custom(
                            descriptor,
                            selected_implementation,
                            starts.clone()
                        )
                        .is_err(),
                        "deserialization and a caller envelope cannot grant maintained registration"
                    );
                } else if evidence == 2 {
                    assert!(
                        RegisteredLanguageModel::custom(
                            descriptor,
                            implementation()?,
                            starts.clone()
                        )
                        .is_err(),
                        "custom descriptor cannot take maintained implementation identity"
                    );
                }
            }
        }
        assert_eq!(starts.0.load(Ordering::SeqCst), 0);
        Ok(())
    }
}
