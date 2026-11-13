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
    configuration: CompiledProviderConfiguration,
    registry: FrozenToolRegistryDescriptor,
    plan: plakous_agent::ResolvedHarnessPlan,
}

impl FrozenSessionPresentation {
    /// The checkpoint's complete configuration; this is data, not fresh authority.
    #[must_use]
    pub fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }

    /// The rehydrated plan, including its model and registry binding facts.
    #[must_use]
    pub fn plan(&self) -> &plakous_agent::ResolvedHarnessPlan {
        &self.plan
    }

    /// The persisted executable name associated with a canonical tool.
    #[must_use]
    pub fn executor_id(&self, name: &ToolName) -> Option<plakous_tools::ToolExecutorId> {
        self.registry.executor_id_for_name(name)
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
        self.registry.check_executor_identities(current)
    }
    /// The selected outer menu, including its execution/caller dispositions.
    #[must_use]
    pub fn selected_tools(&self) -> &[plakous_model::ConfiguredTool] {
        &self.configuration.selected().behavior().tools
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
            configuration: manifest.provider_configuration().clone(),
            registry,
            plan,
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
        self.registry.resolve(&ToolExecutorBindings::capture(live))
    }

    /// Whether this construction registered the named capability.
    #[must_use]
    pub fn contains(&self, name: &ToolName) -> bool {
        self.registry.executor_id_for_name(name).is_some()
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
        selected
            .tools
            .clone_from(&self.configuration.selected().behavior().tools);
        let selected =
            SelectedProviderConfiguration::new(live.selected().route().clone(), selected)
                .map_err(|_| invalid("frozen selected tool configuration is invalid"))?;
        let mut behavior = live.effective().behavior().clone();
        behavior
            .tools
            .clone_from(&self.configuration.effective().behavior().tools);
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            live.effective().descriptor().clone(),
            live.effective().model_implementation().clone(),
            live.effective().authority().clone(),
            behavior,
            live.effective().family_policy().clone(),
        )
        .map_err(|_| invalid("frozen effective tool configuration is invalid"))?;
        CompiledProviderConfiguration::new(selected, effective)
            .map_err(|_| invalid("frozen tool configuration cannot be reconstructed"))
    }
}

fn invalid(message: &str) -> AgentSessionsError {
    AgentSessionsError::new(AgentSessionsErrorKind::IncompatibleBuild, message)
}
