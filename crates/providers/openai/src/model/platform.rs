use super::{
    Arc, CapacityRule, Clock, ConfigError, CredentialReference, EndpointOrigin,
    ModelAdapterProfileId, OPENAI_PLATFORM_ORIGIN, OPENAI_RESPONSES_ADAPTER_REVISION,
    OpenAiKeySource, OpenAiPlatformConfigurationAuthority, OpenAiPlatformModelBuilder,
    OpenAiPlatformModelFacts, OpenAiPlatformResolveError, OpenAiPlatformRouteConfig, OpenAiTarget,
    PlatformStoragePolicy, ResolvedModelDescriptor, SelectedProviderConfiguration,
    StandardOpenAiHttpConfig, SystemClock, admit_reviewed_platform_model,
};

impl OpenAiPlatformModelBuilder {
    /// Captures exact official-Platform facts before constructing an executor.
    #[must_use]
    pub fn new(
        credential: CredentialReference,
        source: Arc<OpenAiKeySource>,
        facts: OpenAiPlatformModelFacts,
        storage: PlatformStoragePolicy,
        project: crate::PlatformProjectPolicy,
    ) -> Self {
        Self {
            credential,
            source,
            facts,
            context_envelope: None,
            storage,
            project,
            http: StandardOpenAiHttpConfig::default(),
            clock: Arc::new(SystemClock),
            admitted_facts: Vec::new(),
        }
    }

    /// Records the product's admitted-fact binding. The bytes are opaque here.
    #[must_use]
    pub fn admitted_facts(mut self, facts: Vec<u8>) -> Self {
        self.admitted_facts = facts;
        self
    }

    /// Declares a custom complete-wire input envelope; this is not a provider capacity guarantee.
    #[must_use]
    pub const fn context_envelope(mut self, maximum: std::num::NonZeroU64) -> Self {
        self.context_envelope = Some(maximum);
        self
    }

    /// Replaces retained transport policy facts.
    #[must_use]
    pub const fn http_config(mut self, http: StandardOpenAiHttpConfig) -> Self {
        self.http = http;
        self
    }

    /// Replaces the retained resolution clock.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Seals immutable Platform configuration authority before execution.
    ///
    /// The result retains exact route, model, project, storage, and HTTP
    /// bounds for configuration compilation. It has no transport client,
    /// credential-exposure operation, or `LanguageModel` implementation.
    pub fn seal(self) -> Result<OpenAiPlatformConfigurationAuthority, ConfigError> {
        let Self {
            credential,
            source,
            facts,
            context_envelope,
            storage,
            project,
            http,
            clock,
            admitted_facts,
        } = self;
        let origin = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN)?;
        let route =
            OpenAiPlatformRouteConfig::new(origin, credential, storage, project)?.resolve(source);
        let (model, family, profile) = facts.identity();
        let admission =
            admit_reviewed_platform_model(&route, &model, &model, Some(&family), &profile)?;
        let admission = if let Some(maximum) = context_envelope {
            crate::manifest::with_context_envelope(admission, &route, maximum)?
        } else {
            admission
        };
        let target = OpenAiTarget::new_platform_manifest_target(
            route,
            admission.descriptor,
            admission.witness,
            clock,
        )?;
        Ok(OpenAiPlatformConfigurationAuthority {
            target,
            http,
            admitted_facts,
        })
    }

    /// Seals explicit fixture authority restricted to exact HTTPS loopback TLS.
    #[doc(hidden)]
    pub fn seal_local_fixture(
        self,
        origin: EndpointOrigin,
    ) -> Result<OpenAiPlatformConfigurationAuthority, ConfigError> {
        let Self {
            credential,
            source,
            facts,
            context_envelope,
            storage,
            project,
            http,
            clock,
            admitted_facts,
        } = self;
        let route =
            OpenAiPlatformRouteConfig::new_local_fixture(origin, credential, storage, project)?
                .resolve(source);
        let (model, family, profile) = facts.identity();
        let admission =
            admit_reviewed_platform_model(&route, &model, &model, Some(&family), &profile)?;
        let admission = if let Some(maximum) = context_envelope {
            crate::manifest::with_context_envelope(admission, &route, maximum)?
        } else {
            admission
        };
        let target = OpenAiTarget::new_platform_manifest_target(
            route,
            admission.descriptor,
            admission.witness,
            clock,
        )?;
        Ok(OpenAiPlatformConfigurationAuthority {
            target,
            http,
            admitted_facts,
        })
    }

    /// Constructs exact production Platform execution with the selected behavior.
    pub fn resolve(
        self,
        selected: SelectedProviderConfiguration,
    ) -> Result<Arc<crate::OpenAiPlatformModel>, OpenAiPlatformResolveError> {
        Ok(Arc::new(crate::OpenAiPlatformModel::new(
            self.seal()?,
            selected,
        )?))
    }
}

impl std::fmt::Debug for OpenAiPlatformModelBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiPlatformModelBuilder")
            .field("credential", &"<redacted reference>")
            .field("source", &"<redacted credential authority>")
            .field("facts", &self.facts)
            .field("context_envelope", &self.context_envelope)
            .field("storage", &self.storage)
            .field("project", &self.project)
            .field("http", &self.http)
            .field("clock", &"<injected>")
            .field("admitted_facts", &self.admitted_facts)
            .finish()
    }
}

impl OpenAiPlatformConfigurationAuthority {
    pub(crate) fn bind_initial_generation(
        self,
        grant: crate::OpenAiKeyGeneration,
    ) -> Result<Self, ConfigError> {
        let descriptor = self.descriptor();
        let clock = self.target.clock();
        let route = self
            .target
            .route()
            .clone()
            .bind_platform_generation(grant)?;
        let mut admission = admit_reviewed_platform_model(
            &route,
            descriptor.requested_model(),
            descriptor
                .reported_model()
                .ok_or(ConfigError::LocalFixtureContext)?,
            descriptor.model_family(),
            descriptor.profile(),
        )?;
        if let Some(context) = descriptor.context_contract()
            && let CapacityRule::ProviderReservedInput {
                maximum_projected_input,
                ..
            } = context.capacity().rule()
            && let Some(maximum) = std::num::NonZeroU64::new(maximum_projected_input)
        {
            admission = crate::manifest::with_context_envelope(admission, &route, maximum)?;
        }
        Ok(Self {
            target: OpenAiTarget::new_platform_manifest_target(
                route,
                admission.descriptor,
                admission.witness,
                clock,
            )?,
            http: self.http,
            admitted_facts: self.admitted_facts,
        })
    }

    pub(crate) fn admitted_facts(&self) -> &[u8] {
        &self.admitted_facts
    }

    /// Returns the exact reviewed descriptor without granting dispatch.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        self.target.descriptor()
    }

    /// Selects complete behavior on this exact provider-owned route and model tuple.
    pub fn selected_configuration(
        &self,
        behavior: plakous_model::ProviderBehaviorConfiguration,
    ) -> Result<SelectedProviderConfiguration, plakous_model::ProviderConfigurationError> {
        use plakous_model::{
            CredentialSelectionId, ProtocolId, ProviderAuthorityClassId, ProviderProfileRevisionId,
            ProviderRouteSelection, RouteId,
        };
        let identity = || -> Result<ProviderRouteSelection, plakous_model::IdentityError> {
            Ok(ProviderRouteSelection::new(
                ProtocolId::new(crate::OPENAI_RESPONSES_PROTOCOL_ID)?,
                RouteId::new(crate::OPENAI_PLATFORM_ROUTE_ID)?,
                self.descriptor().profile().clone(),
                ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?,
                ModelAdapterProfileId::new(crate::OPENAI_PLATFORM_AUTH_PROFILE_ID)?,
                ProviderProfileRevisionId::new(crate::OPENAI_PLATFORM_AUTH_REVISION)?,
                ProviderAuthorityClassId::new(crate::OPENAI_PLATFORM_AUTHORITY_CLASS)?,
                CredentialSelectionId::new(crate::OPENAI_PLATFORM_CREDENTIAL_SELECTION)?,
                self.descriptor().requested_model().clone(),
            ))
        };
        SelectedProviderConfiguration::new(
            identity().map_err(|_| plakous_model::ProviderConfigurationError::Identity)?,
            behavior,
        )
    }

    /// Returns the exact local operation/decoder bounds.
    #[must_use]
    pub const fn http_config(&self) -> StandardOpenAiHttpConfig {
        self.http
    }

    /// Returns the explicit Platform project-header disposition.
    #[must_use]
    pub fn project(&self) -> &crate::PlatformProjectPolicy {
        match self.target.route().platform_project() {
            Some(project) => project,
            None => unreachable!("sealed Platform authority always has a Platform project policy"),
        }
    }

    pub(crate) const fn target(&self) -> &OpenAiTarget {
        &self.target
    }

    pub(crate) const fn is_local_fixture(&self) -> bool {
        self.target.route().is_local_platform_fixture()
    }
}

impl std::fmt::Debug for OpenAiPlatformConfigurationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiPlatformConfigurationAuthority")
            .field("descriptor", self.target.descriptor())
            .field("project", self.project())
            .field("http", &self.http)
            .field("execution", &"not constructed")
            .field("admitted_facts", &self.admitted_facts)
            .finish()
    }
}
