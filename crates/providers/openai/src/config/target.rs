use super::{
    AccessMode, Arc, Capability, Clock, ConfigError, ConformanceStatus, Digest, FactSource,
    ModelAdapterProfileId, ModelId, ModelRequest, OpenAiRoute, OpenAiRouteConformance,
    OpenAiTarget, ResolvedModelDescriptor, ResponsesFeature, ReviewedPlatformManifestWitness,
    Routability, Support, SystemClock, TargetAdmission, ValidatedRequestPlan, fmt,
    validate_reviewed_platform_request, validate_reviewed_platform_witness,
};

impl fmt::Debug for OpenAiTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiTarget")
            .field("route", &self.route)
            .field("descriptor", &self.descriptor)
            .field("routability", &self.routability)
            .field("admission", &self.admission)
            .field("clock", &"<injected clock>")
            .finish()
    }
}

impl OpenAiTarget {
    /// A caller-created descriptor cannot be promoted into a routable target.
    ///
    /// Production Platform targets are issued only through
    /// [`crate::OpenAiPlatformModelBuilder`]. Offline codec targets remain
    /// available through [`Self::new_offline_codec`].
    ///
    /// ```compile_fail
    /// # use plakous_openai::{OpenAiRoute, OpenAiTarget};
    /// # use plakous_model::ResolvedModelDescriptor;
    /// # fn bypass(route: OpenAiRoute, descriptor: ResolvedModelDescriptor) {
    /// let _ = OpenAiTarget::new(route, descriptor);
    /// # }
    /// ```
    pub(crate) fn new_platform_manifest_target(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        witness: ReviewedPlatformManifestWitness,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if !matches!(
            route.access_mode(),
            AccessMode::PlatformApi | AccessMode::LocalFixture
        ) {
            return Err(ConfigError::PlatformTargetRequiresManifestWitness);
        }
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        let target = Self {
            route,
            descriptor,
            routability: Routability::Routable,
            admission: TargetAdmission::ReviewedPlatform(witness),
            clock,
        };
        target.validate_current_route_evidence()?;
        Ok(target)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn new_native_catalog_target(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        witness: super::NativeCodexCatalogWitness,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if !route.is_native_codex() {
            return Err(ConfigError::NativeTargetRequiresCatalogWitness);
        }
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        validate_conformance_claims(route.conformance(), &descriptor, clock.now())?;
        match &route.auth {
            super::RouteAuth::NativeCodexSubscription { authority } => authority
                .validate_catalog_witness(&descriptor, &witness)
                .map_err(|_| ConfigError::NativeTargetRequiresCatalogWitness)?,
            super::RouteAuth::Platform { .. } => {
                return Err(ConfigError::NativeTargetRequiresCatalogWitness);
            }
        }
        Ok(Self {
            route,
            descriptor,
            routability: Routability::Routable,
            admission: TargetAdmission::NativeCatalog(witness),
            clock,
        })
    }

    /// Binds fixture-backed codec evidence without creating a routable target.
    ///
    /// This constructor exists only for deterministic request and stream
    /// conformance work. Every positive descriptor fact must be labelled
    /// [`FactSource::TestFixture`], and the resulting target remains explicitly
    /// [`Routability::NonRoutable`] even when its underlying route has credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign protocol or route, a non-fixture positive
    /// claim, an excluded feature, or an unknown feature without codec evidence.
    pub fn new_offline_codec(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
    ) -> Result<Self, ConfigError> {
        Self::new_offline_codec_with_clock(route, descriptor, Arc::new(SystemClock))
    }

    /// Binds fixture-backed codec evidence to an explicit deterministic clock.
    ///
    /// The resulting target is always non-routable.
    ///
    /// # Errors
    ///
    /// Returns the same validation failures as [`Self::new_offline_codec`].
    pub fn new_offline_codec_with_clock(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        validate_offline_codec_claims(route.conformance(), &descriptor)?;
        Ok(Self {
            route,
            descriptor,
            routability: Routability::NonRoutable,
            admission: TargetAdmission::OfflineCodec,
            clock,
        })
    }

    /// Returns the resolved route.
    #[must_use]
    pub const fn route(&self) -> &OpenAiRoute {
        &self.route
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn validate_route(&self, route: &OpenAiRoute) -> Result<(), super::AuthError> {
        if self.route.id() != route.id()
            || self.route.origin() != route.origin()
            || self.route.responses_path() != route.responses_path()
            || self.route.dialect() != route.dialect()
            || self.route.storage() != route.storage()
            || self.route.account_binding != route.account_binding
            || self.route.credential_authority_binding != route.credential_authority_binding
        {
            return Err(super::AuthError::BindingMismatch);
        }
        Ok(())
    }

    #[cfg(feature = "native-auth")]
    pub(crate) const fn route_id(&self) -> &super::RouteId {
        self.route.id()
    }

    pub(crate) const fn account_binding(&self) -> Digest {
        self.route.account_binding
    }

    pub(crate) const fn credential_authority_binding(&self) -> Digest {
        self.route.credential_authority_binding
    }
    /// Returns the route-scoped model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        self.descriptor.requested_model()
    }

    /// Returns the independently selected harness profile.
    #[must_use]
    pub const fn profile(&self) -> &ModelAdapterProfileId {
        self.descriptor.profile()
    }

    /// Reports whether this exact target may be used for live dispatch.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        self.routability
    }

    /// Returns the complete resolved descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    #[cfg(feature = "native-auth")]
    pub(crate) const fn codex_responses_lite(&self) -> bool {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => witness.responses_lite(),
            _ => false,
        }
    }

    pub(crate) fn validate_current_route_evidence(&self) -> Result<(), ConfigError> {
        match &self.admission {
            TargetAdmission::ReviewedPlatform(witness) => {
                validate_reviewed_platform_witness(&self.route, &self.descriptor, witness)?;
                validate_feature_conformance(self.route.conformance(), &self.descriptor)
            }
            #[cfg(feature = "native-auth")]
            TargetAdmission::NativeCatalog(witness) => {
                match &self.route.auth {
                    super::RouteAuth::NativeCodexSubscription { authority } => authority
                        .validate_catalog_witness(&self.descriptor, witness)
                        .map_err(|_| ConfigError::NativeTargetRequiresCatalogWitness)?,
                    super::RouteAuth::Platform { .. } => {
                        return Err(ConfigError::NativeTargetRequiresCatalogWitness);
                    }
                }
                validate_conformance_claims(
                    self.route.conformance(),
                    &self.descriptor,
                    self.clock.now(),
                )
            }
            TargetAdmission::OfflineCodec => Err(ConfigError::NonRoutableTarget),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) async fn revalidate_native_runtime_authority(
        &self,
        checked_at: super::Timestamp,
    ) -> Result<super::RuntimeModelAuthority, super::RuntimeModelAuthorityError> {
        let (
            super::RouteAuth::NativeCodexSubscription { authority },
            TargetAdmission::NativeCatalog(witness),
        ) = (&self.route.auth, &self.admission)
        else {
            return Err(super::RuntimeModelAuthorityError::Unavailable);
        };
        if !self.route.is_native_codex()
            || self.descriptor.protocol() != self.route.dialect().protocol()
            || self.descriptor.route() != self.route.id()
            || self.descriptor.profile().as_str() != super::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        {
            return Err(super::RuntimeModelAuthorityError::Invalid);
        }
        authority
            .revalidate_runtime_authority(&self.descriptor, witness, checked_at)
            .await
    }

    pub(crate) fn validate_request_intent(
        &self,
        request: &ModelRequest,
    ) -> Result<(), ConfigError> {
        match &self.admission {
            TargetAdmission::ReviewedPlatform(witness) => {
                validate_reviewed_platform_request(&self.descriptor, witness, request)
            }
            #[cfg(feature = "native-auth")]
            TargetAdmission::NativeCatalog(_) => super::validate_unscoped_adapter_request(request),
            TargetAdmission::OfflineCodec => Err(ConfigError::NonRoutableTarget),
        }
    }

    pub(crate) fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// The admitting catalog's observation and expiry.
    #[cfg(feature = "native-auth")]
    pub(crate) const fn native_codex_catalog_window(
        &self,
    ) -> Option<(super::Timestamp, super::Timestamp)> {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => {
                Some((witness.observed_at(), witness.expires_at()))
            }
            _ => None,
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn native_codex_credential_selection(
        &self,
    ) -> Option<(&super::CredentialFence, Digest)> {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => Some((
                witness.credential_fence(),
                witness.credential_selection_epoch(),
            )),
            _ => None,
        }
    }

    /// Verifies that a plan was validated for this exact immutable binding.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign route/model/profile plan.
    pub fn validate_plan(&self, plan: &ValidatedRequestPlan) -> Result<(), ConfigError> {
        self.descriptor
            .validate_plan_binding(plan)
            .map_err(|_| ConfigError::ForeignPlan)
    }
}

#[cfg(feature = "native-auth")]
fn validate_conformance_claims(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
    now: super::Timestamp,
) -> Result<(), ConfigError> {
    let native_support = native_option_namespace_support(descriptor)?;
    for (capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() == FactSource::TestFixture {
            return Err(ConfigError::FixtureClaimRequiresOfflineTarget);
        }
        let pinned_codex_parallel = descriptor.profile().as_str()
            == super::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            && *capability == Capability::ParallelTools
            && support.provenance().source() == FactSource::BuiltIn
            && support.provenance().expires_at().is_none();
        if support.provenance().source() != FactSource::RouteEvidence && !pinned_codex_parallel {
            return Err(ConfigError::RoutableCapabilityRequiresRouteEvidence);
        }
        if support.provenance().is_expired(now) {
            return Err(ConfigError::RoutableCapabilityEvidenceExpired);
        }
    }
    for support in &native_support {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() == FactSource::TestFixture {
            return Err(ConfigError::FixtureClaimRequiresOfflineTarget);
        }
        if support.provenance().source() != FactSource::RouteEvidence {
            return Err(ConfigError::RoutableCapabilityRequiresRouteEvidence);
        }
        if support.provenance().is_expired(now) {
            return Err(ConfigError::RoutableCapabilityEvidenceExpired);
        }
    }
    validate_feature_conformance(conformance, descriptor)
}

fn validate_feature_conformance(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
) -> Result<(), ConfigError> {
    let native_support = native_option_namespace_support(descriptor)?;
    descriptor
        .capabilities()
        .get(Capability::Streaming)
        .filter(|support| support.support() != Support::Unsupported)
        .ok_or(ConfigError::StreamingConformanceRequired)?;
    for (&capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        let feature = capability_feature(capability)?;
        match conformance.feature(feature) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    for support in native_support {
        if support.support() == Support::Unsupported {
            continue;
        }
        match conformance.feature(ResponsesFeature::NativeOptions) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    Ok(())
}

fn validate_offline_codec_claims(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
) -> Result<(), ConfigError> {
    for (&capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() != FactSource::TestFixture {
            return Err(ConfigError::NonFixtureOfflineClaim);
        }
        let feature = capability_feature(capability)?;
        match conformance.feature(feature) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown if conformance.has_fixture_codec_evidence(feature) => {}
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    for support in native_option_namespace_support(descriptor)? {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() != FactSource::TestFixture {
            return Err(ConfigError::NonFixtureOfflineClaim);
        }
        match conformance.feature(ResponsesFeature::NativeOptions) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown
                if conformance.has_fixture_codec_evidence(ResponsesFeature::NativeOptions) => {}
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    Ok(())
}

fn native_option_namespace_support(
    descriptor: &ResolvedModelDescriptor,
) -> Result<Vec<plakous_model::FeatureSupport>, ConfigError> {
    let capabilities =
        serde_json::to_value(descriptor.capabilities()).map_err(|_| ConfigError::BindingDigest)?;
    let namespaces = capabilities
        .get("native_option_namespaces")
        .and_then(serde_json::Value::as_object)
        .ok_or(ConfigError::BindingDigest)?;
    namespaces
        .values()
        .cloned()
        .map(|support| serde_json::from_value(support).map_err(|_| ConfigError::BindingDigest))
        .collect()
}

fn capability_feature(capability: Capability) -> Result<ResponsesFeature, ConfigError> {
    match capability {
        Capability::Streaming => Ok(ResponsesFeature::HttpSse),
        Capability::Reasoning => Ok(ResponsesFeature::ReasoningOutput),
        Capability::Tools => Ok(ResponsesFeature::ClientFunctionCalls),
        Capability::ParallelTools => Ok(ResponsesFeature::ParallelFunctionCalls),
        Capability::StructuredOutput => Ok(ResponsesFeature::StructuredOutput),
        Capability::Continuation => Ok(ResponsesFeature::PreviousResponseId),
        Capability::Caching => Ok(ResponsesFeature::PromptCaching),
        Capability::NativeOptions => Ok(ResponsesFeature::NativeOptions),
        Capability::ImageInput => Ok(ResponsesFeature::ImageInput),
        Capability::HostedProgrammaticToolCalling => Ok(ResponsesFeature::ProgrammaticCallers),
        _ => Err(ConfigError::UnknownConformanceClaim),
    }
}
