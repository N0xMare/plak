use super::{
    Arc, CompiledProviderConfiguration, LanguageModel, ModelImplementationIdentity,
    ProviderConfigurationError, ResolvedModelDescriptor, RuntimeModelAuthority,
    StandardIntegrationAuthority, validate_integration_ownership,
};

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
