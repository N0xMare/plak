//! Host reconstruction of frozen data against current executable capabilities.

use super::{AgentSessionsError, AgentSessionsErrorKind};
use plakous_model::{
    CompiledProviderConfiguration, EffectiveProviderConfiguration, SelectedProviderConfiguration,
    ToolName,
};
use plakous_tools::{FrozenToolRegistry, FrozenToolRegistryDescriptor, ToolExecutorBindings};

/// Construction data only; never credentials or executable authority.
/// This host-layer seam has no supported embedder stability claim.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct FrozenSessionPresentation {
    configuration: Option<CompiledProviderConfiguration>,
    registry: Option<FrozenToolRegistryDescriptor>,
    plan: Option<plakous_agent::ResolvedHarnessPlan>,
    semantic: Option<SemanticSessionPresentation>,
}

/// Unsupported host-move data. The export document, not a native codec, owns its encoding.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct SemanticSessionPresentation {
    pub registrations: Vec<plakous_tools::SemanticToolRegistration>,
    pub selected: Vec<plakous_model::ConfiguredTool>,
    pub effective: Vec<plakous_model::ConfiguredTool>,
}

impl FrozenSessionPresentation {
    /// Reconstructs data-only overrides; current executors are checked when the registry builds.
    ///
    /// # Errors
    /// Refuses malformed or duplicate capability/executable identities.
    pub fn from_semantic(data: SemanticSessionPresentation) -> Result<Self, AgentSessionsError> {
        let mut names = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        if data.registrations.len() > 4096
            || data.selected.len() > 4096
            || data.effective.len() > 4096
        {
            return Err(invalid("semantic tool presentation is oversized"));
        }
        for record in &data.registrations {
            plakous_tools::RegisteredToolId::new(record.registered_id.clone())
                .map_err(|_| invalid("invalid semantic capability identity"))?;
            plakous_tools::ToolExecutorId::new(record.executor_id.clone())
                .map_err(|_| invalid("invalid semantic executable identity"))?;
            if !names.insert(record.definition.name()) || !ids.insert(&record.registered_id) {
                return Err(invalid("duplicate semantic tool presentation"));
            }
        }
        Ok(Self {
            configuration: None,
            registry: None,
            plan: None,
            semantic: Some(data),
        })
    }

    /// Captures the closed semantic presentation without source plans, hashes or registry bytes.
    ///
    /// # Errors
    /// Refuses incomplete presentation data.
    pub fn semantic(&self) -> Result<SemanticSessionPresentation, AgentSessionsError> {
        if let Some(data) = &self.semantic {
            return Ok(data.clone());
        }
        let configuration = self
            .configuration
            .as_ref()
            .ok_or_else(|| invalid("missing presentation configuration"))?;
        Ok(SemanticSessionPresentation {
            registrations: self
                .registry
                .as_ref()
                .ok_or_else(|| invalid("missing presentation registry"))?
                .semantic_registrations(),
            selected: configuration.selected().behavior().tools.clone(),
            effective: configuration.effective().behavior().tools.clone(),
        })
    }
    /// The session's presented canonical definition, including nested tools.
    #[must_use]
    pub fn definition(&self, name: &ToolName) -> Option<&plakous_model::ToolDefinition> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.definition(name))
            .or_else(|| {
                self.semantic
                    .as_ref()?
                    .registrations
                    .iter()
                    .find(|record| record.definition.name() == name)
                    .map(|record| &record.definition)
            })
    }
    /// Data-only requirements; no executable authority is reconstructed.
    ///
    /// # Errors
    /// Returns unreadable stored metadata.
    pub fn executor_requirements(
        &self,
    ) -> Result<
        Vec<(
            ToolName,
            plakous_tools::ToolExecutorId,
            plakous_tools::ToolExecutorBindingIdentity,
        )>,
        AgentSessionsError,
    > {
        if let Some(registry) = &self.registry {
            return registry
                .executor_requirements()
                .map_err(|_| invalid("stored executable requirements are unreadable"));
        }
        self.semantic
            .as_ref()
            .ok_or_else(|| invalid("missing presentation data"))?
            .registrations
            .iter()
            .map(|record| {
                Ok((
                    record.definition.name().clone(),
                    plakous_tools::ToolExecutorId::new(record.executor_id.clone())
                        .map_err(|_| invalid("invalid semantic executable identity"))?,
                    record.executor_binding,
                ))
            })
            .collect()
    }
    /// The checkpoint's complete configuration; this is data, not fresh authority.
    #[must_use]
    pub const fn configuration(&self) -> Option<&CompiledProviderConfiguration> {
        self.configuration.as_ref()
    }

    /// The rehydrated plan, including its model and registry binding facts.
    #[must_use]
    pub const fn plan(&self) -> Option<&plakous_agent::ResolvedHarnessPlan> {
        self.plan.as_ref()
    }

    /// The persisted executable name associated with a canonical tool.
    #[must_use]
    pub fn executor_id(&self, name: &ToolName) -> Option<plakous_tools::ToolExecutorId> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.executor_id_for_name(name))
            .or_else(|| {
                self.semantic
                    .as_ref()?
                    .registrations
                    .iter()
                    .find(|record| record.definition.name() == name)
                    .and_then(|record| {
                        plakous_tools::ToolExecutorId::new(record.executor_id.clone()).ok()
                    })
            })
    }

    /// Checks release-owned executable identities without starting any executor.
    ///
    /// # Errors
    /// Returns exact missing/revised binding or descriptor failures.
    pub fn check_executor_identities(
        &self,
        current: impl FnMut(
            &ToolName,
            &plakous_tools::ToolExecutorId,
        ) -> Option<plakous_tools::ToolExecutorBindingIdentity>,
    ) -> Result<(), plakous_tools::ToolBindingError> {
        let mut current = current;
        if let Some(registry) = &self.registry {
            return registry.check_executor_identities(current);
        }
        for record in &self
            .semantic
            .as_ref()
            .ok_or(plakous_tools::ToolDescriptorError::InvalidSemantics)?
            .registrations
        {
            let id = plakous_tools::ToolExecutorId::new(record.executor_id.clone())
                .map_err(|_| plakous_tools::ToolDescriptorError::InvalidSemantics)?;
            let found = current(record.definition.name(), &id).ok_or_else(|| {
                plakous_tools::ToolBindingError::MissingBinding { id: id.clone() }
            })?;
            if !found.accepts(&id, record.executor_binding) {
                return Err(plakous_tools::ToolBindingError::BindingIdentityMismatch {
                    id,
                    expected: record.executor_binding,
                    found,
                });
            }
        }
        Ok(())
    }
    /// The selected outer menu, including its execution/caller dispositions.
    #[must_use]
    pub fn selected_tools(&self) -> &[plakous_model::ConfiguredTool] {
        match (&self.configuration, &self.semantic) {
            (Some(configuration), _) => &configuration.selected().behavior().tools,
            (_, Some(semantic)) => &semantic.selected,
            _ => &[],
        }
    }

    pub(super) fn from_checkpoint(
        checkpoint: &super::RuntimeCheckpoint,
    ) -> Result<Self, AgentSessionsError> {
        let manifest = checkpoint.manifest();
        let component = manifest.tools().registry();
        let registry = match component.codec_revision() {
            plakous_tools::TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
                FrozenToolRegistryDescriptor::decode_v1(component.bytes())
            }
            plakous_tools::TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
                FrozenToolRegistryDescriptor::decode_v2(component.bytes())
            }
            _ => return Err(invalid("stored tool registry codec is unsupported")),
        }
        .map_err(|_| invalid("stored tool registry is unreadable"))?;
        let plan = plakous_agent::internal::facade_support::checkpoint_harness_plan(checkpoint)
            .map_err(|error| super::runtime_error(&error))?;
        if plan.tool_registry_digest() != registry.registry_identity() {
            return Err(
                invalid("stored harness plan is bound to a different tool registry")
                    .with_binding_change(plakous_agent::BindingChange::ToolRegistry),
            );
        }
        let exposure = manifest.tools().exposure();
        match exposure.codec_revision() {
            plakous_tools::TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION => {
                plakous_tools::FrozenToolExposureDescriptor::decode_v1(exposure.bytes(), &registry)
                    .map(|_| ())
            }
            plakous_tools::TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION => {
                plakous_tools::FrozenToolProjectionDescriptorV2::decode_v2(
                    exposure.bytes(),
                    &registry,
                )
                .map(|_| ())
            }
            _ => return Err(invalid("stored tool presentation codec is unsupported")),
        }
        .map_err(|_| invalid("stored tool presentation is unreadable"))?;
        Ok(Self {
            configuration: Some(manifest.provider_configuration().clone()),
            registry: Some(registry),
            plan: Some(plan),
            semantic: None,
        })
    }

    /// Restores definitions, never old executor code. Missing or revised live
    /// executor bindings refuse before any invocation.
    ///
    /// # Errors
    /// Returns the precise executor binding or descriptor refusal.
    pub fn restore_registry(
        &self,
        live: &FrozenToolRegistry,
    ) -> Result<FrozenToolRegistry, plakous_tools::ToolBindingError> {
        if let Some(registry) = &self.registry {
            return registry.resolve(&ToolExecutorBindings::capture(live));
        }
        FrozenToolRegistryDescriptor::restore_semantic_registrations(
            &self
                .semantic
                .as_ref()
                .ok_or(plakous_tools::ToolDescriptorError::InvalidSemantics)?
                .registrations,
            live,
        )
    }

    /// Whether this construction registered the named capability.
    #[must_use]
    pub fn contains(&self, name: &ToolName) -> bool {
        self.executor_id(name).is_some()
    }

    /// Restores only selected/effective tool data. Every other fresh model,
    /// route, authority and policy fact still participates in exact validation.
    ///
    /// # Errors
    /// Returns invalid configuration when current authority cannot compile it.
    pub fn restore_configuration(
        &self,
        live: &CompiledProviderConfiguration,
    ) -> Result<CompiledProviderConfiguration, AgentSessionsError> {
        let mut selected = live.selected().behavior().clone();
        selected.tools = self.selected_tools().to_vec();
        let selected =
            SelectedProviderConfiguration::new(live.selected().route().clone(), selected)
                .map_err(|_| invalid("frozen selected tool configuration is invalid"))?;
        let mut behavior = live.effective().behavior().clone();
        behavior.tools = match (&self.configuration, &self.semantic) {
            (Some(configuration), _) => configuration.effective().behavior().tools.clone(),
            (_, Some(semantic)) => semantic.effective.clone(),
            _ => return Err(invalid("missing semantic effective presentation")),
        };
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            live.effective().descriptor().clone(),
            live.effective().model_implementation().clone(),
            live.effective().authority().clone(),
            behavior,
            live.effective().family_policy().clone(),
        )
        .map_err(|_| invalid("frozen effective tool configuration is invalid"))?;
        let restored = CompiledProviderConfiguration::new(selected, effective)
            .map_err(|_| invalid("frozen tool configuration cannot be reconstructed"))?;
        if self.configuration.as_ref().is_some_and(|source| {
            restored.selected_identity() != source.selected_identity()
                || restored.effective_identity() != source.effective_identity()
        }) {
            return Err(
                invalid("frozen presentation cannot hide changed provider configuration")
                    .with_binding_change(plakous_agent::BindingChange::ProviderConfiguration),
            );
        }
        Ok(restored)
    }
}

fn invalid(message: &str) -> AgentSessionsError {
    AgentSessionsError::new(AgentSessionsErrorKind::IncompatibleBuild, message)
}
