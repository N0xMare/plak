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

mod compiled;
pub use compiled::CompiledProviderConfiguration;
mod runtime;
pub use runtime::{ConfiguredLanguageModel, RegisteredLanguageModel};

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
mod tests;
