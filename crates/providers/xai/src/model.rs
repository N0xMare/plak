//! Route-scoped xAI model and provisional Grok profile identity.

use std::collections::BTreeMap;

#[cfg(feature = "native-auth")]
use plakous_model::{
    Capability, CapabilityConstraints, FactProvenance, FactSource, FeatureSupport, Support,
};
use plakous_model::{
    CapabilitySet, IdentityError, ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId,
    ResolvedModelDescriptor, RouteId, Timestamp,
};
use thiserror::Error;

use crate::{XaiDialect, XaiGrokSubscriptionRoute, XaiRouteKind};

/// A route-independent xAI model identity and family classification.
///
/// Constructing a model never changes or selects a route. The caller must bind
/// it to an explicit [`XaiGrokSubscriptionRoute`] when resolving a descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiModel {
    remote_id: ModelId,
    family: Option<ModelFamilyId>,
}

impl XaiModel {
    /// Creates a model from an exact remote ID and optional family.
    #[must_use]
    pub const fn new(remote_id: ModelId, family: Option<ModelFamilyId>) -> Self {
        Self { remote_id, family }
    }

    /// Returns the exact model ID sent to xAI.
    #[must_use]
    pub const fn remote_id(&self) -> &ModelId {
        &self.remote_id
    }

    /// Returns the optional conservative family classification.
    #[must_use]
    pub const fn family(&self) -> Option<&ModelFamilyId> {
        self.family.as_ref()
    }
}

/// A provisional Grok coding-profile marker.
///
/// Profile instructions and tool exposure policy are deliberately deferred.
/// This marker therefore has no mutable content or exposure map that could
/// drift without changing its immutable identity, and makes no harness-quality
/// capability claim.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GrokCodingProfile {
    /// An unsupported marker used only to prove fail-closed profile admission.
    #[default]
    Provisional,
    /// Exact reviewed `SuperGrok` compatibility profile.
    PinnedCompatibility,
}

impl GrokCodingProfile {
    /// Creates the content-free provisional profile marker.
    #[must_use]
    pub const fn provisional() -> Self {
        Self::Provisional
    }

    /// Creates the immutable reviewed `SuperGrok` compatibility profile marker.
    #[must_use]
    pub const fn pinned_compatibility() -> Self {
        Self::PinnedCompatibility
    }

    /// Returns the immutable profile ID.
    ///
    /// # Errors
    ///
    /// Returns an identity error only if the built-in ID violates canonical rules.
    pub fn profile_id(self) -> Result<ModelAdapterProfileId, IdentityError> {
        match self {
            Self::Provisional => ModelAdapterProfileId::new("xai.grok-coding.profile-deferred-v1"),
            Self::PinnedCompatibility => {
                ModelAdapterProfileId::new("xai.grok.compat.2026-07-31.r1")
            }
        }
    }
}

/// An exact routable route/model/profile binding and its neutral descriptor.
#[derive(Clone, Debug)]
pub struct XaiResolvedModel {
    route: XaiGrokSubscriptionRoute,
    model: XaiModel,
    descriptor: ResolvedModelDescriptor,
}

impl XaiResolvedModel {
    /// Resolves a model/profile against one already configured routable route.
    ///
    /// The resulting neutral descriptor uses the route's dynamic identity,
    /// which binds origin, account, access mode, and credential authority.
    /// Only route-specific positive evidence enters the live descriptor. Codec
    /// fixtures never promote a routable capability.
    ///
    /// # Errors
    ///
    /// Returns [`XaiModelError::NonRoutableSubscription`] for the blocked
    /// subscription selection, or an identity error for invalid built-in IDs.
    pub fn new(
        route: XaiGrokSubscriptionRoute,
        model: XaiModel,
        profile: GrokCodingProfile,
        observed_at: Timestamp,
    ) -> Result<Self, XaiModelError> {
        let Some(identity) = route.identity() else {
            return Err(XaiModelError::NonRoutableSubscription);
        };
        validate_live_admission(&route, &model, profile, observed_at)?;
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new(route.dialect().protocol_id())?,
            RouteId::new(identity.kind().route_id())?,
            model.remote_id.clone(),
            Some(model.remote_id.clone()),
            model.family.clone(),
            profile.profile_id()?,
            route_capabilities(&route, &model, profile),
        );
        #[cfg(feature = "native-auth")]
        let descriptor = {
            let admission = route
                .native_catalog_admission()
                .ok_or(XaiModelError::UnadmittedSubscriptionModel)?;
            let row = admission
                .entry(model.remote_id())
                .ok_or(XaiModelError::UnadmittedSubscriptionModel)?;
            match crate::context::build_context_contract(
                model.remote_id().as_str(),
                u64::from(row.diagnostics().reported_context_length().get()),
                identity.account_binding(),
                identity.credential_authority_binding(),
                admission.observed_at(),
                admission.expires_at(),
                admission
                    .receipt_identity(identity)
                    .map_err(|_| XaiModelError::UnadmittedSubscriptionModel)?,
            )
            .map_err(|_| XaiModelError::UnadmittedSubscriptionModel)?
            {
                Some(contract) => descriptor.with_context_contract(contract),
                None => descriptor,
            }
            .with_same_served_models(
                crate::native_auth::grok_enrollment(model.remote_id().as_str())
                    .map(|row| row.same_served_models)
                    .unwrap_or_default()
                    .iter()
                    .map(|served| ModelId::new(*served))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        Ok(Self {
            route,
            model,
            descriptor,
        })
    }

    /// Returns the exact selected and credential-bearing route.
    #[must_use]
    pub const fn route(&self) -> &XaiGrokSubscriptionRoute {
        &self.route
    }

    /// Returns the exact remote model identity.
    #[must_use]
    pub const fn model(&self) -> &XaiModel {
        &self.model
    }

    /// Returns the immutable neutral descriptor used to validate every request plan.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Binds a renewed receipt's route to this admitted target. The fresh
    /// resolution of the same model must keep the route identity and the
    /// semantic descriptor; the admitted descriptor is kept, so plans and the
    /// compiled configuration stay bound. Returns `None` on any change.
    #[cfg(feature = "native-auth")]
    pub(crate) fn renewed(&self, route: XaiGrokSubscriptionRoute) -> Option<Self> {
        let observed_at = route.native_catalog_admission()?.observed_at();
        let fresh = Self::new(
            route,
            self.model.clone(),
            GrokCodingProfile::pinned_compatibility(),
            observed_at,
        )
        .ok()?;
        let same_semantics = fresh.descriptor.semantic_binding_digest().ok()?
            == self.descriptor.semantic_binding_digest().ok()?;
        (same_semantics
            && fresh.route.identity() == self.route.identity()
            && fresh.route.dialect() == self.route.dialect())
        .then(|| Self {
            route: fresh.route,
            model: self.model.clone(),
            descriptor: self.descriptor.clone(),
        })
    }
}

fn route_capabilities(
    route: &XaiGrokSubscriptionRoute,
    model: &XaiModel,
    profile: GrokCodingProfile,
) -> CapabilitySet {
    #[cfg(feature = "native-auth")]
    let mut features = BTreeMap::new();
    #[cfg(not(feature = "native-auth"))]
    let features = {
        let _ = (route, model, profile);
        BTreeMap::new()
    };
    #[cfg(feature = "native-auth")]
    if profile == GrokCodingProfile::PinnedCompatibility
        && let Some(admission) = route.native_catalog_admission()
        && let Ok(profile_id) = profile.profile_id()
        && admission.admits(model.remote_id(), &profile_id)
        && crate::native_auth::is_subscription_coding_model_id(model.remote_id().as_str())
    {
        let supports_reasoning =
            crate::native_auth::subscription_model_supports_reasoning(model.remote_id().as_str());
        let evidence_note = format!(
            "{}; exact reviewed model {}; application-local function tools only; fixed profile {}; pinned OpenCode source {}; {} {}; non-normative mutable documentation locators: function calling {}, model page {}; adapter {}; authenticated exact-ID catalog receipt; no aliases, hosted tools, media, structured output, continuation, or catalog-derived context authority; reasoning={}",
            crate::responses_dialect::SUPERGROK_FEATURE_REVISION,
            model.remote_id().as_str(),
            crate::native_auth::XAI_GROK_COMPATIBILITY_PROFILE,
            crate::native_auth::XAI_OPENCODE_SOURCE_REVISION,
            if model.remote_id().as_str() == crate::native_auth::XAI_GROK_47_CODING_MODEL
                || model.remote_id().as_str() == crate::native_auth::XAI_GROK_47_FAST_CODING_MODEL
            {
                "reviewed model enrollment source"
            } else {
                "pinned Grok Build source"
            },
            crate::native_auth::model_catalog_source_revision(model.remote_id().as_str()),
            crate::native_auth::XAI_FUNCTION_CALLING_DOCUMENTATION_LOCATOR,
            crate::native_auth::XAI_GROK_BUILD_MODEL_DOCUMENTATION_LOCATOR,
            crate::XAI_RESPONSES_ADAPTER_REVISION,
            if supports_reasoning {
                "route-evidenced model-specific effort table"
            } else {
                "not-route-supported"
            },
        );
        let provenance = FactProvenance::new(
            FactSource::RouteEvidence,
            admission.observed_at(),
            Some(admission.expires_at()),
        );
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(None, Some(evidence_note.clone())),
                provenance,
            ),
        );
        features.insert(
            Capability::Tools,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::new(None, Some(evidence_note.clone())),
                provenance,
            ),
        );
        if supports_reasoning {
            features.insert(
                Capability::Reasoning,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::new(None, Some(evidence_note)),
                    provenance,
                ),
            );
        }
    }
    CapabilitySet::new(features, BTreeMap::new())
}

#[cfg_attr(
    not(feature = "native-auth"),
    allow(
        clippy::unnecessary_wraps,
        reason = "the feature-independent caller preserves the native-auth validation result"
    )
)]
fn validate_live_admission(
    route: &XaiGrokSubscriptionRoute,
    model: &XaiModel,
    profile: GrokCodingProfile,
    observed_at: Timestamp,
) -> Result<(), XaiModelError> {
    #[cfg(feature = "native-auth")]
    {
        let admission = route.native_catalog_admission();
        if admission.is_some() || profile == GrokCodingProfile::PinnedCompatibility {
            let Some(admission) = admission else {
                return Err(XaiModelError::UnadmittedSubscriptionModel);
            };
            if profile != GrokCodingProfile::PinnedCompatibility {
                return Err(XaiModelError::UnadmittedSubscriptionModel);
            }
            let profile_id = profile.profile_id()?;
            if !admission.admits(model.remote_id(), &profile_id)
                || model
                    .family()
                    .is_some_and(|family| family.as_str() != "responses-family")
                || observed_at < admission.observed_at()
                || observed_at >= admission.expires_at()
            {
                return Err(XaiModelError::UnadmittedSubscriptionModel);
            }
        }
    }
    #[cfg(not(feature = "native-auth"))]
    let _ = (route, model, profile, observed_at);
    Ok(())
}

/// A route/model/profile resolution failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum XaiModelError {
    /// The selected subscription route has no configured OAuth credential source.
    #[error("xai.grok_subscription remains non-routable")]
    NonRoutableSubscription,
    /// A native subscription model lacked the exact fresh catalog/evidence tuple.
    #[error("the xAI subscription model is not admitted by fresh exact evidence")]
    UnadmittedSubscriptionModel,
    /// A canonical route, protocol, model, or profile identity was invalid.
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// Returns whether a route kind and dialect form a public built-in pair.
#[must_use]
pub const fn dialect_matches_route(route: XaiRouteKind, dialect: XaiDialect) -> bool {
    matches!(
        (route, dialect),
        (
            XaiRouteKind::PlatformApiKey,
            XaiDialect::PlatformResponses2026_08
        ) | (
            XaiRouteKind::GrokSubscription,
            XaiDialect::GrokSubscriptionResponses2026_07
        )
    )
}

#[cfg(feature = "native-auth")]
mod shared_host_model {
    use std::fmt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use futures_util::lock::Mutex as AsyncMutex;
    use plakous_attempt::LedgerPolicy;
    #[cfg(test)]
    use plakous_model::{
        CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
        CountCertificateKind, CountObservation, DigestDomain, EvidenceFreshness,
        MeasurementCertificate, SafeLimit,
    };
    use plakous_model::{
        Clock, Digest, LanguageModel, ModelPreparationContext, ModelPreparationError,
        ModelPreparationErrorKind, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
        PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId,
        ReasoningPolicy, ResolvedModelDescriptor, RuntimeModelAuthority,
        RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SystemClock, Timestamp,
        ValidatedRequestPlan,
    };
    use plakous_responses::{
        CanonicalIdSources, ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamLimits,
    };
    use plakous_responses_dialect::{
        PreparedResponsesModelExecution, ResponsesArtifactReplayBridge,
        ResponsesAttemptDialectData, ResponsesAttemptService, ResponsesAttemptTarget,
        ResponsesAuthSlots, ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData,
        ResponsesProviderErrorPolicy, ResponsesRequestAuthority, ResponsesStoragePolicy,
        canonical_local_tools, is_unprojected_string_custom_tool_definition,
        prepare_responses_attempt, start_prepared_responses_model_call,
    };
    use thiserror::Error;

    use crate::native_auth::{
        SuperGrokDispatchGuard, SuperGrokResponsesExecution, revalidate_supergrok_responses,
        supergrok_credential_rejected, supergrok_responses_preflight,
    };
    use crate::{
        SUPERGROK_RESPONSES_DIALECT, XaiResolvedModel,
        responses_dialect::SUPERGROK_RESPONSES_DIALECT_REVISION,
    };

    const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
    const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 64;
    const PREFLIGHT_REVISION: u32 = 1;
    const FINAL_AUTHORIZATION_REVISION: u32 = 1;
    const RESPONSES_FAMILY: &str = "responses-family";
    const GROK_PROJECTION_REVISION: &str = crate::context::GROK_CONTEXT_PROJECTION_REVISION;
    const GROK_ACCOUNTING_REVISION: &str = crate::context::GROK_CONTEXT_ACCOUNTING_REVISION;
    #[cfg(test)]
    const TEST_MEASUREMENT_CERTIFICATE_ID: &str = "xai-test-complete-wire-byte-bound";
    #[cfg(test)]
    const TEST_MEASUREMENT_CERTIFICATE_REVISION: &str = "fixture-v1";

    #[cfg(test)]
    impl XaiResolvedModel {
        pub(crate) fn with_test_context_contract(
            mut self,
            observed_at: Timestamp,
        ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
            let expires_at = Timestamp::new(observed_at.seconds().saturating_add(3_600), 0)?;
            let freshness = EvidenceFreshness::new(observed_at, Some(expires_at))?;
            let corpus = Digest::sha256(
                DigestDomain::Blob,
                1,
                &[b"xai-test-complete-wire-byte-bound-corpus-v1"],
            )?;
            let capacity = CapacityCertificate::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                "xai-test-capacity",
                "fixture-v1",
                ContextEvidenceSource::ReleaseCertificate,
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(10_000_000),
                },
                None,
                freshness,
            )?;
            let measurement = MeasurementCertificate::standard(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                TEST_MEASUREMENT_CERTIFICATE_ID,
                TEST_MEASUREMENT_CERTIFICATE_REVISION,
                "complete-wire-byte-upper-bound",
                "fixture-v1",
                ContextEvidenceSource::ReleaseCertificate,
                CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
                None,
                freshness,
            )?;
            self.descriptor = self
                .descriptor
                .clone()
                .with_context_contract(ContextContract::new(
                    ProtocolId::new(RESPONSES_FAMILY)?,
                    GROK_ACCOUNTING_REVISION,
                    GROK_PROJECTION_REVISION,
                    "xai-test-output-reservation-v1",
                    capacity,
                    vec![measurement],
                )?);
            Ok(self)
        }
    }

    /// Immutable logical-operation and shared stream bounds for SuperGrok.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct StandardXaiHttpConfig {
        operation_timeout: Duration,
        event_channel_capacity: usize,
        responses_limits: ResponsesStreamLimits,
    }

    impl Default for StandardXaiHttpConfig {
        fn default() -> Self {
            Self {
                operation_timeout: DEFAULT_OPERATION_TIMEOUT,
                event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
                responses_limits: ResponsesStreamLimits::default(),
            }
        }
    }

    impl StandardXaiHttpConfig {
        /// Creates explicit nonzero operation, delivery, and decoder bounds.
        pub fn new(
            operation_timeout: Duration,
            event_channel_capacity: usize,
            responses_limits: ResponsesStreamLimits,
        ) -> Result<Self, XaiLanguageModelConfigError> {
            if operation_timeout.is_zero() || event_channel_capacity == 0 {
                return Err(XaiLanguageModelConfigError::InvalidPolicy);
            }
            Ok(Self {
                operation_timeout,
                event_channel_capacity,
                responses_limits,
            })
        }

        /// Returns the dispatch timeout: it bounds preflight, retry waits and
        /// each exchange up to its response head, never a streaming body.
        #[must_use]
        pub const fn operation_timeout(self) -> Duration {
            self.operation_timeout
        }

        /// Returns the bounded observation capacity.
        #[must_use]
        pub const fn event_channel_capacity(self) -> usize {
            self.event_channel_capacity
        }

        /// Returns the shared decoder limits.
        #[must_use]
        pub const fn responses_limits(self) -> ResponsesStreamLimits {
            self.responses_limits
        }
    }

    /// Failure to construct a shared-host SuperGrok model.
    #[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
    #[non_exhaustive]
    pub enum XaiLanguageModelConfigError {
        /// The route is not an admitted native SuperGrok subscription route.
        #[error("xAI shared-host execution requires an admitted SuperGrok route")]
        UnsupportedRoute,
        /// A duration or queue bound was zero.
        #[error("xAI shared-host policy bounds must be nonzero")]
        InvalidPolicy,
        /// Shared Responses construction facts were inconsistent.
        #[error("xAI shared Responses construction failed")]
        SharedBinding,
    }

    /// Native SuperGrok model backed exclusively by the shared Responses host.
    ///
    /// The admitted descriptor is immutable. The credential receipt behind it
    /// is renewed before dispatch when the shared store refreshed the grant.
    pub struct XaiLanguageModel {
        target: Mutex<Arc<SuperGrokDispatchTarget>>,
        renewal: AsyncMutex<()>,
        renewal_floor: crate::XaiRenewalFloor,
        admitted_from: Timestamp,
        admitted_until: Timestamp,
        client: plakous_http::Client,
        config: StandardXaiHttpConfig,
        attempt_data: ResponsesAttemptDialectData,
        descriptor: ResolvedModelDescriptor,
        clock: Arc<dyn Clock>,
        #[cfg(test)]
        request_gate: Option<Arc<dyn plakous_auth::subscription_limit::SubscriptionRequestGate>>,
    }

    /// One exact credential receipt bound to the shared Responses host. A
    /// renewal replaces the whole target; a prepared call keeps its own.
    struct SuperGrokDispatchTarget {
        resolved: XaiResolvedModel,
        execution: Arc<SuperGrokResponsesExecution>,
        service: Arc<ResponsesAttemptService<SuperGrokResponsesExecution, SuperGrokDispatchGuard>>,
    }

    type DispatchService =
        Arc<ResponsesAttemptService<SuperGrokResponsesExecution, SuperGrokDispatchGuard>>;

    fn supergrok_authorization() -> Result<
        ResponsesAuthorizationAdapter<SuperGrokResponsesExecution, SuperGrokDispatchGuard>,
        XaiLanguageModelConfigError,
    > {
        Ok(ResponsesAuthorizationAdapter::new(
            PREFLIGHT_REVISION,
            FINAL_AUTHORIZATION_REVISION,
            supergrok_responses_preflight,
            revalidate_supergrok_responses,
        )
        .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?
        .with_credential_rejected(supergrok_credential_rejected)
        .with_transport_failed(SuperGrokDispatchGuard::transport_failed))
    }

    fn dispatch_service(
        client: &plakous_http::Client,
        attempt_data: &ResponsesAttemptDialectData,
        execution: &Arc<SuperGrokResponsesExecution>,
    ) -> Result<DispatchService, XaiLanguageModelConfigError> {
        Ok(Arc::new(
            ResponsesAttemptService::new(
                client.clone(),
                Arc::clone(execution),
                attempt_data.clone(),
                supergrok_authorization()?,
            )
            .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?,
        ))
    }

    impl XaiLanguageModel {
        /// Binds one exact native subscription target to the shared family host.
        pub fn new(
            resolved: XaiResolvedModel,
            client: plakous_http::Client,
        ) -> Result<Self, XaiLanguageModelConfigError> {
            Self::with_config(resolved, client, StandardXaiHttpConfig::default())
        }

        /// Binds one exact target with explicit transport/decoder bounds.
        pub fn with_config(
            resolved: XaiResolvedModel,
            client: plakous_http::Client,
            config: StandardXaiHttpConfig,
        ) -> Result<Self, XaiLanguageModelConfigError> {
            let execution = SuperGrokResponsesExecution::new(resolved.clone(), client.clone())
                .ok_or(XaiLanguageModelConfigError::UnsupportedRoute)?;
            Self::with_execution(resolved, client, config, execution)
        }

        fn with_execution(
            resolved: XaiResolvedModel,
            client: plakous_http::Client,
            config: StandardXaiHttpConfig,
            execution: SuperGrokResponsesExecution,
        ) -> Result<Self, XaiLanguageModelConfigError> {
            let route = resolved.route();
            if !route.is_routable()
                || route.responses_path() != Some(crate::SUPERGROK_RESPONSES_PATH)
                || config.operation_timeout.is_zero()
                || config.event_channel_capacity == 0
            {
                return Err(XaiLanguageModelConfigError::UnsupportedRoute);
            }
            // The descriptor's context evidence is valid only inside the
            // admitting catalog's window; renewal never carries a target out
            // of it.
            let admission = route
                .native_catalog_admission()
                .ok_or(XaiLanguageModelConfigError::UnsupportedRoute)?;
            let (admitted_from, admitted_until) = (admission.observed_at(), admission.expires_at());
            let descriptor = resolved.descriptor().clone();
            let replay = SUPERGROK_RESPONSES_DIALECT
                .replay_projection(
                    descriptor.protocol().clone(),
                    descriptor.model_family().cloned(),
                    descriptor.requested_model().clone(),
                )
                .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?;
            let authorization = supergrok_authorization()?;
            let response_limits = plakous_responses_dialect::ResponsesResponseLimits::default();
            let target = ResponsesAttemptTarget::from_admitted_target(
                &descriptor,
                route
                    .identity()
                    .ok_or(XaiLanguageModelConfigError::UnsupportedRoute)?
                    .account_binding(),
                supergrok_semantic_revisions()?,
            )
            .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?;
            let attempt_data = ResponsesAttemptDialectData::new(
                SUPERGROK_RESPONSES_DIALECT_REVISION,
                target,
                &client,
                &authorization,
                LedgerPolicy::RESPONSES_FAMILY,
                ResponsesAuthSlots::Primary,
                SUPERGROK_RESPONSES_DIALECT.request_policy(),
                SUPERGROK_RESPONSES_DIALECT.stream_data(descriptor.requested_model()),
                config.responses_limits,
                replay,
                ResponsesStorageMode::Stateless,
                ResponsesProviderErrorPolicy::Grok,
                response_limits,
                None,
                false,
            )
            .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?;
            let execution = Arc::new(execution);
            let service = dispatch_service(&client, &attempt_data, &execution)?;
            Ok(Self {
                target: Mutex::new(Arc::new(SuperGrokDispatchTarget {
                    resolved,
                    execution,
                    service,
                })),
                renewal: AsyncMutex::new(()),
                renewal_floor: crate::XaiRenewalFloor::default(),
                admitted_from,
                admitted_until,
                client,
                config,
                attempt_data,
                descriptor,
                clock: Arc::new(SystemClock),
                #[cfg(test)]
                request_gate: None,
            })
        }

        #[cfg(test)]
        pub(crate) fn with_subscription_request_gate(
            self,
            gate: Arc<dyn plakous_auth::subscription_limit::SubscriptionRequestGate>,
        ) -> Result<Self, XaiLanguageModelConfigError> {
            let resolved = self
                .current_target()
                .map_err(|_| XaiLanguageModelConfigError::SharedBinding)?
                .resolved
                .clone();
            let mut execution =
                SuperGrokResponsesExecution::new(resolved.clone(), self.client.clone())
                    .ok_or(XaiLanguageModelConfigError::UnsupportedRoute)?;
            execution.set_request_gate(Arc::clone(&gate));
            let mut model = Self::with_execution(resolved, self.client, self.config, execution)?;
            model.request_gate = Some(gate);
            Ok(model)
        }

        /// Returns the immutable admitted descriptor.
        #[must_use]
        pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
            &self.descriptor
        }

        /// The session's hosted web and X search side-request executor on
        /// this exact route, login, model and client, at the lowest effort
        /// the model's ladder admits.
        #[must_use]
        pub fn retrieval(self: &Arc<Self>) -> crate::XaiRetrieval {
            let effort = plakous_responses_dialect::lowest_responses_effort(
                crate::subscription_model_reasoning_effort_options(
                    self.descriptor.requested_model().as_str(),
                ),
            );
            crate::XaiRetrieval::supergrok(Arc::clone(self), effort)
        }

        pub(crate) const fn client(&self) -> &plakous_http::Client {
            &self.client
        }

        /// Revalidates admission exactly as before a model round (renewing a
        /// stale receipt), then authorizes one physical side request
        /// carrying `body` on the current receipt.
        pub(crate) async fn side_request(
            self: &Arc<Self>,
            body: &[u8],
        ) -> Result<(plakous_http::Request, SuperGrokDispatchGuard), &'static str> {
            Arc::clone(self)
                .revalidate_runtime_authority(self.clock.now())
                .await
                .map_err(|_| "SuperGrok admission is no longer current")?;
            let target = self
                .current_target()
                .map_err(|_| "SuperGrok target is unavailable")?;
            crate::native_auth::supergrok_side_request(&target.execution, body).await
        }

        fn current_target(
            &self,
        ) -> Result<Arc<SuperGrokDispatchTarget>, RuntimeModelAuthorityError> {
            self.target
                .lock()
                .map(|target| Arc::clone(&target))
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)
        }

        fn revalidate_target(
            &self,
            target: &SuperGrokDispatchTarget,
            checked_at: Timestamp,
        ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
            if checked_at >= self.admitted_until {
                crate::route::log_authority_rejection("route.admitted_window");
                return Err(RuntimeModelAuthorityError::Stale);
            }
            target.resolved.route().revalidate_runtime_authority(
                &self.descriptor,
                target.resolved.model().remote_id(),
                self.descriptor.profile(),
                checked_at,
            )
        }

        /// Replaces a stale receipt before any dispatch: one claimed refresh
        /// and exact catalog resolution of the same selection, accepted only
        /// for the same route identity and model semantics. Concurrent callers
        /// note their `checked_at`, wait for one renewal and then check the
        /// receipt it installed, whose observation never postdates the earliest
        /// noted check. A floor left by a cancelled renewal only lowers a later
        /// stamp inside the admitted window.
        async fn renew(
            &self,
            stale: Arc<SuperGrokDispatchTarget>,
            checked_at: Timestamp,
        ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
            // Outside the admitted window no renewal can help: refuse before
            // any refresh, catalog request or wait.
            if checked_at < self.admitted_from || checked_at >= self.admitted_until {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            self.renewal_floor.note(checked_at);
            let _renewal = self.renewal.lock().await;
            // A renewal that failed while this caller waited cleared the floor.
            self.renewal_floor.note(checked_at);
            let current = self.current_target()?;
            if !Arc::ptr_eq(&current, &stale) {
                return self.revalidate_target(&current, checked_at);
            }
            let Some(authority) = stale.resolved.route().credential_authority() else {
                crate::route::log_authority_rejection("route.refresh_unavailable");
                return Err(RuntimeModelAuthorityError::Stale);
            };
            let renewed = authority.renew(&self.renewal_floor).await;
            self.renewal_floor.clear();
            let renewed = renewed.map_err(|error| {
                crate::route::log_authority_rejection("route.renew");
                match error {
                    crate::XaiAuthError::BindingMismatch => {
                        RuntimeModelAuthorityError::ForeignAuthority
                    }
                    _ => RuntimeModelAuthorityError::Stale,
                }
            })?;
            let route = stale
                .resolved
                .route()
                .renewed(renewed)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            let resolved = stale.resolved.renewed(route).ok_or_else(|| {
                crate::route::log_authority_rejection("route.renewed_target");
                RuntimeModelAuthorityError::ForeignAuthority
            })?;
            let execution = SuperGrokResponsesExecution::new(resolved.clone(), self.client.clone())
                .ok_or(RuntimeModelAuthorityError::ForeignAuthority)?;
            #[cfg(test)]
            let execution = {
                let mut execution = execution;
                if let Some(gate) = &self.request_gate {
                    execution.set_request_gate(Arc::clone(gate));
                }
                execution
            };
            let execution = Arc::new(execution);
            let service = dispatch_service(&self.client, &self.attempt_data, &execution)
                .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            let target = Arc::new(SuperGrokDispatchTarget {
                resolved,
                execution,
                service,
            });
            let authority = self.revalidate_target(&target, checked_at)?;
            *self
                .target
                .lock()
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)? = target;
            Ok(authority)
        }
    }

    impl fmt::Debug for XaiLanguageModel {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("XaiLanguageModel")
                .field("resolved", &"<redacted exact SuperGrok target>")
                .field("client", &self.client)
                .field("config", &self.config)
                .field("execution", &"shared Responses host")
                .finish_non_exhaustive()
        }
    }

    impl LanguageModel for XaiLanguageModel {
        fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
            let origin = plakous_http::Origin::parse(
                self.current_target()
                    .ok()?
                    .resolved
                    .route()
                    .origin()?
                    .as_str(),
            )
            .ok()?;
            Some(plakous_responses_dialect::bind_reachability_probe(
                self.client.clone(),
                origin,
                self.current_target()
                    .ok()?
                    .resolved
                    .descriptor()
                    .route()
                    .clone(),
            ))
        }

        fn revalidate_runtime_authority(
            self: Arc<Self>,
            checked_at: Timestamp,
        ) -> RuntimeModelAuthorityOperation {
            Box::pin(async move {
                let target = self.current_target()?;
                match self.revalidate_target(&target, checked_at) {
                    Err(RuntimeModelAuthorityError::Stale) => self.renew(target, checked_at).await,
                    result => result,
                }
            })
        }

        fn prepare_call(
            self: Arc<Self>,
            plan: ValidatedRequestPlan,
            context: ModelPreparationContext,
        ) -> Result<PreparedModelCall, ModelPreparationError> {
            if let Some(projection) = plan.request().tool_projection() {
                let target = projection.target();
                if target.protocol() != self.descriptor.protocol().as_str()
                    || target.integration_profile() != self.descriptor.profile().as_str()
                    || target.family()
                        != self
                            .descriptor
                            .model_family()
                            .map_or("", plakous_model::ModelFamilyId::as_str)
                    || target.requested_model() != self.descriptor.requested_model().as_str()
                    || target.effective_model() != self.descriptor.requested_model().as_str()
                {
                    return Err(ModelPreparationError::rejected(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "SuperGrok compiled tool projection does not match the admitted exact target",
                    ));
                }
            }
            self.descriptor.validate_plan_binding(&plan)?;
            self.descriptor
                .validate_plan_admission(plan.request(), &plan)?;
            context
                .authority()
                .validate(&self.descriptor, context.prepared_at())?;
            let target = self.current_target()?;
            let selected = self.revalidate_target(&target, context.prepared_at())?;
            if !selected.same_selected_authority(context.authority()) {
                return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
            }

            let request = plan.request();
            let effort = match request.reasoning() {
                ReasoningPolicy::Disabled => None,
                ReasoningPolicy::Enabled { effort, .. } => effort,
                _ => {
                    return Err(preparation_rejection(
                        ModelPreparationErrorKind::InvalidRequest,
                        "SuperGrok reasoning policy is not admitted",
                    ));
                }
            };
            let exact_efforts = crate::subscription_model_reasoning_effort_options(
                target.resolved.model().remote_id().as_str(),
            );
            if effort
                .as_ref()
                .is_some_and(|effort| !exact_efforts.contains(effort))
                || !SUPERGROK_RESPONSES_DIALECT.supports_reasoning_effort(effort.as_ref())
            {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "SuperGrok reasoning effort is not admitted",
                ));
            }
            if request.tools().iter().any(|tool| {
                is_unprojected_string_custom_tool_definition(request, tool.definition())
            }) {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "SuperGrok does not admit custom tools",
                ));
            }
            let canonical_tools = canonical_local_tools(request);
            let expected_model = self
                .descriptor
                .reported_model()
                .unwrap_or(self.descriptor.requested_model())
                .clone();
            let canonical = ResponsesCanonicalDialectData::grok(expected_model, canonical_tools)
                .map_err(|_| {
                    preparation_rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "SuperGrok canonical projection facts are invalid",
                    )
                })?;
            let authority = request_authority(
                &target.resolved,
                &plan,
                context.authority(),
                context
                    .cache_affinity()
                    .map(plakous_model::ModelCacheAffinity::identity),
            )?;
            let replay_bridge = active_replay_bridge(
                context.active_family_artifact(),
                request,
                authority.clone(),
                context.prepared_at(),
            )?;
            let artifact_binding = authority.artifact_binding().clone();
            let prepared_attempt = prepare_responses_attempt(
                authority,
                request,
                replay_bridge
                    .as_ref()
                    .map(ResponsesArtifactReplayBridge::context),
                &self.attempt_data,
            )
            .map_err(|error| {
                preparation_rejection(
                    error.preparation_error_kind(),
                    format!("SuperGrok request lowering or serialization failed: {error}"),
                )
            })?;
            let wire_digest = prepared_attempt.wire_digest();
            let wire_size = u64::try_from(prepared_attempt.wire_size()).map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::ResourceLimit,
                    "SuperGrok prepared request exceeds addressable size",
                )
            })?;
            let count_observation = match self.descriptor.context_contract() {
                None => None,
                #[cfg(test)]
                Some(contract)
                    if contract.measurements().iter().any(|certificate| {
                        certificate.certificate_id() == TEST_MEASUREMENT_CERTIFICATE_ID
                    }) =>
                {
                    Some(
                        CountObservation::new(
                            TEST_MEASUREMENT_CERTIFICATE_ID,
                            TEST_MEASUREMENT_CERTIFICATE_REVISION,
                            wire_size,
                        )
                        .map_err(|_| {
                            preparation_rejection(
                                ModelPreparationErrorKind::InvalidConfiguration,
                                "SuperGrok test measurement fixture is invalid",
                            )
                        })?,
                    )
                }
                Some(_) => {
                    let counts = prepared_attempt
                        .wire_occurrence_counts()
                        .copied()
                        .ok_or_else(|| {
                            preparation_rejection(
                                ModelPreparationErrorKind::Projection,
                                "SuperGrok wire grammar is outside the release-verified estimator",
                            )
                        })?;
                    Some(
                        crate::context::count_observation(
                            target.resolved.model().remote_id().as_str(),
                            wire_size,
                            counts,
                        )
                        .map_err(|_| {
                            preparation_rejection(
                                ModelPreparationErrorKind::ResourceLimit,
                                "SuperGrok context upper-bound estimation failed",
                            )
                        })?,
                    )
                }
            };
            let token_projection =
                PreparedTokenProjection::new(wire_digest, wire_size, count_observation)?
                    .with_input_parts(plakous_model::ContextInputParts::measure_wire(
                        prepared_attempt.body().as_ref(),
                    ))
                    .with_wire_input(
                        prepared_attempt.body().as_ref(),
                        Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                        &context,
                    )?;
            #[cfg(test)]
            let fixture_contract = self.descriptor.context_contract().is_some_and(|contract| {
                contract.measurements().iter().any(|certificate| {
                    certificate.certificate_id() == TEST_MEASUREMENT_CERTIFICATE_ID
                })
            });
            #[cfg(not(test))]
            let fixture_contract = false;
            let output_limit = if fixture_contract {
                prepared_attempt
                    .enforced_output_tokens()
                    .map(|tokens| {
                        PreparedOutputLimit::enforced_wire_maximum(
                            tokens,
                            "max_output_tokens",
                            wire_digest,
                            crate::XAI_RESPONSES_ADAPTER_REVISION,
                        )
                    })
                    .transpose()?
            } else {
                None
            };
            let cache_use = match (
                context.active_family_artifact(),
                context
                    .cache_affinity()
                    .map(plakous_model::ModelCacheAffinity::identity),
            ) {
                (Some(artifact), Some(affinity)) => {
                    PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                        artifact: artifact.integrity(),
                        affinity,
                    }
                }
                (Some(artifact), None) => PreparedCacheUse::ActiveFamilyArtifact {
                    artifact: artifact.integrity(),
                },
                (None, Some(affinity)) => PreparedCacheUse::RuntimeAffinity { affinity },
                (None, None) => PreparedCacheUse::None,
            };
            let projection = PreparedModelProjection::new(
                &self.descriptor,
                &plan,
                &context,
                ProtocolId::new(RESPONSES_FAMILY).map_err(|_| {
                    preparation_rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "Responses protocol-family identity is invalid",
                    )
                })?,
                crate::XAI_RESPONSES_ADAPTER_REVISION,
                GROK_PROJECTION_REVISION,
                GROK_ACCOUNTING_REVISION,
                wire_digest,
                wire_size,
                token_projection,
                cache_use,
                output_limit,
            )?;
            let record =
                PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
            let execution = PreparedResponsesModelExecution::new(
                prepared_attempt,
                canonical,
                CanonicalIdSources::with_clock(Arc::clone(&self.clock)),
                CanonicalIdSources::with_clock(Arc::clone(&self.clock)),
                replay_bridge
                    .as_ref()
                    .map(|bridge| bridge.hosted_programs().clone())
                    .unwrap_or_default(),
                plan,
                artifact_binding,
                Arc::clone(&self.clock),
                self.descriptor.clone(),
                self.config.event_channel_capacity(),
                None,
            )
            .map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "SuperGrok prepared execution facts are inconsistent",
                )
            })?;
            let service = Arc::clone(&target.service);
            let clock = Arc::clone(&self.clock);
            let timeout = self.config.operation_timeout();
            Ok(PreparedModelCall::new(record, move |call_context| {
                let deadline = operation_deadline(clock.as_ref(), timeout, None);
                start_prepared_responses_model_call(execution, service, call_context, deadline)
            }))
        }
    }

    fn request_authority(
        resolved: &XaiResolvedModel,
        plan: &ValidatedRequestPlan,
        runtime: &RuntimeModelAuthority,
        cache_affinity: Option<Digest>,
    ) -> Result<ResponsesRequestAuthority, ModelPreparationError> {
        let identity = resolved.route().identity().ok_or_else(|| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "SuperGrok route identity is unavailable",
            )
        })?;
        if identity.account_binding() != runtime.account_binding() {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        ResponsesRequestAuthority::from_admitted_operation(
            resolved.descriptor(),
            plan,
            plan.request(),
            runtime.account_binding(),
            supergrok_semantic_revisions().map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "SuperGrok semantic revisions are invalid",
                )
            })?,
            cache_affinity,
            ResponsesStoragePolicy::Stateless,
        )
        .map_err(|error| {
            let message = match error {
                plakous_responses_dialect::ResponsesReplayError::IncompleteAuthority => {
                    "SuperGrok request authority is incomplete for the admitted operation"
                }
                plakous_responses_dialect::ResponsesReplayError::AuthorityConstruction => {
                    "SuperGrok request authority identity construction failed"
                }
                _ => "SuperGrok request authority does not match the admitted operation",
            };
            preparation_rejection(ModelPreparationErrorKind::InvalidRequest, message)
        })
    }

    fn supergrok_semantic_revisions()
    -> Result<ResponsesSemanticRevisions, XaiLanguageModelConfigError> {
        ResponsesSemanticRevisions::new(5, 4, 5)
            .map_err(|_| XaiLanguageModelConfigError::SharedBinding)
    }

    fn operation_deadline(
        clock: &dyn Clock,
        timeout: Duration,
        requested: Option<Timestamp>,
    ) -> Instant {
        let now = Instant::now();
        let configured = now.checked_add(timeout).unwrap_or(now);
        let Some(requested) = requested else {
            return configured;
        };
        let wall_now = clock.now();
        if requested <= wall_now {
            return now;
        }
        now.checked_add(Duration::from_secs(
            requested.seconds().saturating_sub(wall_now.seconds()),
        ))
        .map_or(configured, |deadline| deadline.min(configured))
    }

    fn active_replay_bridge(
        active: Option<&plakous_model::ActiveFamilyArtifact>,
        request: &plakous_model::ModelRequest,
        authority: ResponsesRequestAuthority,
        now: Timestamp,
    ) -> Result<Option<ResponsesArtifactReplayBridge>, ModelPreparationError> {
        let Some(active) = active else {
            return Ok(None);
        };
        match ResponsesArtifactReplayBridge::from_active_artifact(
            active,
            request,
            authority,
            SUPERGROK_RESPONSES_DIALECT.output_item_replay(),
            Some(SUPERGROK_RESPONSES_DIALECT.opaque_output_replay()),
            None,
            now,
        ) {
            Ok(bridge) => Ok(Some(bridge)),
            // The artifact continues a different canonical prefix or was
            // produced under a different stable prefix, so it is not this
            // request's to reuse. Prepare fresh rather than refusing: a
            // compaction fold, a qualification probe and a replaced turn all
            // legitimately differ. The tool-free operation-budget wrap-up does
            // not: the binding leaves tool selection out, so it keeps replay.
            Err(error) if error.artifact_is_not_for_this_request() => Ok(None),
            Err(error) => Err(preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                format!("SuperGrok active Responses artifact is not reusable: {error}"),
            )),
        }
    }

    fn preparation_rejection(
        kind: ModelPreparationErrorKind,
        message: impl AsRef<str>,
    ) -> ModelPreparationError {
        ModelPreparationError::rejected(kind, message)
    }

    #[cfg(test)]
    fn covered_projection_after_output(
        plan: &ValidatedRequestPlan,
        output: &[plakous_model::ConversationItem],
    ) -> Result<plakous_model::CanonicalItemPrefixIdentity, std::io::Error> {
        plan.request()
            .input()
            .staged_canonical_item_prefix_identity(output)
            .map_err(std::io::Error::other)
    }

    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "unit tests use bounded fixture assertions"
    )]
    mod tests {
        use std::collections::BTreeMap;
        use std::error::Error;
        use std::str::FromStr;

        use plakous_model::{
            ActiveFamilyArtifact, Actor, AttemptId, CanonicalHistory, Capability,
            CapabilityConstraints, CapabilitySet, ContentBlock, ConversationView,
            DataClassification, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
            FactProvenance, FactSource, FamilyArtifactProducerFacts, FeatureSupport, FinishDetails,
            FinishReason, HistorySegment, HistorySegmentKind, ModelAdapterProfileId, ModelFamilyId,
            ModelId, ModelRequest, ModelResponse, OpaqueBlock, OpaqueFallback, ProtocolId,
            Provenance, ReplayEnvelope, ReplayKindId, ReplayScope, RequestMetadata, RouteId, RunId,
            SealedFamilyArtifactCandidate, SelectedProviderConfigurationId, Support, TextBlock,
            TextPhase, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolName,
            ToolResultBlock, ToolResultKind, ToolResultPart, TurnId, ValidationMode,
            WorkingProjection,
        };
        use plakous_responses::{
            RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION,
            ResponsesArtifact, ResponsesArtifactBinding, ResponsesArtifactCodec,
            ResponsesArtifactProduction, ResponsesArtifactReplayKind, ResponsesReplayContract,
            ResponsesReplayItem, ResponsesReplayPurpose,
        };
        use plakous_responses_dialect::{
            ResponsesAttemptBuildError, ResponsesRequestConfig, lower_responses_request,
        };
        use serde_json::json;
        use serde_json::value::RawValue;

        use super::*;

        type TestResult<T = ()> = Result<T, Box<dyn Error>>;

        const MODEL: &str = "grok-build-0.1";

        #[test]
        fn native_operation_budget_still_obeys_earlier_caller_deadline() -> TestResult {
            let clock = plakous_model::FixedClock::new(Timestamp::new(1_000, 0)?);
            let before = Instant::now();
            let full = operation_deadline(&clock, Duration::from_secs(300), None);
            let earlier = operation_deadline(
                &clock,
                Duration::from_secs(300),
                Some(Timestamp::new(1_180, 0)?),
            );
            assert!(full.duration_since(before) >= Duration::from_secs(299));
            assert!(earlier.duration_since(before) >= Duration::from_secs(179));
            assert!(earlier.duration_since(before) < Duration::from_secs(181));
            assert!(earlier < full);
            Ok(())
        }

        fn now() -> Timestamp {
            Timestamp::new(1_800_000_000, 0).expect("fixed time")
        }

        fn test_descriptor(model: &str, route: &str) -> TestResult<ResolvedModelDescriptor> {
            let mut features = BTreeMap::new();
            features.insert(
                Capability::Streaming,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    FactProvenance::new(FactSource::TestFixture, now(), None),
                ),
            );
            Ok(ResolvedModelDescriptor::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                ProtocolId::new(SUPERGROK_RESPONSES_DIALECT.protocol_id())?,
                RouteId::new(route)?,
                ModelId::new(model)?,
                Some(ModelId::new(model)?),
                Some(ModelFamilyId::new("grok")?),
                ModelAdapterProfileId::new("xai.grok.compat.2026-07-31.r1")?,
                CapabilitySet::new(features, BTreeMap::new()),
            ))
        }

        fn harness_plan() -> TestResult<Digest> {
            Ok(Digest::sha256(
                DigestDomain::ResolvedHarnessPlan,
                1,
                &[b"xai-model-artifact-test-plan"],
            )?)
        }

        fn provider_configuration_ids() -> TestResult<(
            SelectedProviderConfigurationId,
            EffectiveProviderConfigurationId,
        )> {
            Ok((
                serde_json::from_value(serde_json::to_value(Digest::sha256(
                    DigestDomain::SelectedProviderConfiguration,
                    5,
                    &[b"xai-model-test-selected-configuration"],
                )?)?)?,
                serde_json::from_value(serde_json::to_value(Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"xai-model-test-effective-configuration"],
                )?)?)?,
            ))
        }

        fn metadata(round: u32) -> TestResult<RequestMetadata> {
            let suffix =
                |offset: u32| format!("00000000-0000-4000-8000-{:012}", round * 10 + offset);
            Ok(RequestMetadata::for_turn(
                RunId::from_str("00000000-0000-4000-8000-000000000001")?,
                TurnId::from_str(&suffix(1))?,
                AttemptId::from_str(&suffix(2))?,
                now(),
            ))
        }

        fn user_segment(round: u32) -> TestResult<HistorySegment> {
            let id = format!("00000000-0000-4000-8000-{:012}", 100 + round);
            let item = plakous_model::ConversationItem::new(
                EntryId::from_str(&id)?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(
                    format!("intent {round}"),
                    None,
                )?)],
                Provenance::UserInput,
            )?;
            Ok(HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![item],
            )?)
        }

        fn request(segments: Vec<HistorySegment>, round: u32) -> TestResult<ModelRequest> {
            let (selected, effective) = provider_configuration_ids()?;
            Ok(ModelRequest::builder(
                selected,
                effective,
                ConversationView::from_shared(
                    CanonicalHistory::new(segments)?,
                    WorkingProjection::default(),
                ),
                metadata(round)?,
            )
            .resolved_harness_plan_identity(harness_plan()?)
            .build()?)
        }

        fn tool_free_request(
            segments: Vec<HistorySegment>,
            round: u32,
        ) -> TestResult<ModelRequest> {
            let (selected, effective) = provider_configuration_ids()?;
            Ok(ModelRequest::builder(
                selected,
                effective,
                ConversationView::from_shared(
                    CanonicalHistory::new(segments)?,
                    WorkingProjection::default(),
                ),
                metadata(round)?,
            )
            .resolved_harness_plan_identity(harness_plan()?)
            .tool_choice(plakous_model::ToolChoice::None)
            .build()?)
        }

        fn account() -> TestResult<Digest> {
            Ok(Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"xai-model-artifact-test-account"],
            )?)
        }

        fn authority(
            descriptor: &ResolvedModelDescriptor,
            plan: &ValidatedRequestPlan,
            revisions: ResponsesSemanticRevisions,
        ) -> TestResult<ResponsesRequestAuthority> {
            Ok(ResponsesRequestAuthority::from_admitted_operation(
                descriptor,
                plan,
                plan.request(),
                account()?,
                revisions,
                Some(Digest::sha256(
                    DigestDomain::CacheAffinity,
                    1,
                    &[b"xai-model-artifact-test-affinity"],
                )?),
                ResponsesStoragePolicy::Stateless,
            )?)
        }

        fn replay_envelope(
            descriptor: &ResolvedModelDescriptor,
            raw: serde_json::Value,
            kind: plakous_responses_dialect::ResponsesReplayKind<'_>,
        ) -> TestResult<ReplayEnvelope> {
            Ok(ReplayEnvelope::new(
                descriptor.protocol().clone(),
                descriptor.model_family().cloned(),
                ReplayScope::ExactModel(descriptor.requested_model().clone()),
                ReplayKindId::new(kind.name())?,
                kind.version(),
                DataClassification::Sensitive,
                RawValue::from_string(raw.to_string())?,
            )?)
        }

        fn canonical_output(
            descriptor: &ResolvedModelDescriptor,
            round: u32,
        ) -> TestResult<(
            Vec<plakous_model::ConversationItem>,
            Vec<ResponsesReplayItem>,
        )> {
            let output_kind = SUPERGROK_RESPONSES_DIALECT.output_item_replay();
            let opaque_kind = SUPERGROK_RESPONSES_DIALECT.opaque_output_replay();
            let reasoning = replay_envelope(
                descriptor,
                json!({
                    "id":format!("reasoning_{round}"),
                    "type":"reasoning",
                    "status":"completed",
                    "summary":[],
                    "encrypted_content":format!("encrypted-{round}")
                }),
                output_kind,
            )?;
            let message = replay_envelope(
                descriptor,
                json!({
                    "id":format!("message_{round}"),
                    "type":"message",
                    "status":"completed",
                    "role":"assistant",
                    "content":[{
                        "type":"output_text",
                        "text":format!("answer {round}"),
                        "annotations":[]
                    }]
                }),
                output_kind,
            )?;
            let future = replay_envelope(
                descriptor,
                json!({
                    "id":format!("future_{round}"),
                    "type":"future_output",
                    "status":"completed",
                    "payload":{"opaque":format!("opaque-{round}")}
                }),
                opaque_kind,
            )?;
            let entry = |offset: u32| {
                EntryId::from_str(&format!(
                    "00000000-0000-4000-8000-{:012}",
                    1_000 + round * 10 + offset
                ))
            };
            let output = vec![
                plakous_model::ConversationItem::new(
                    entry(1)?,
                    Actor::Assistant,
                    vec![ContentBlock::Opaque(OpaqueBlock::new(
                        reasoning.clone(),
                        OpaqueFallback::CompatibleReplay,
                    )?)],
                    Provenance::ModelOutput,
                )?,
                plakous_model::ConversationItem::new(
                    entry(2)?,
                    Actor::Assistant,
                    vec![
                        ContentBlock::Text(TextBlock::new(
                            format!("answer {round}"),
                            Some(TextPhase::Final),
                        )?),
                        ContentBlock::Opaque(OpaqueBlock::new(
                            message.clone(),
                            OpaqueFallback::CompatibleReplay,
                        )?),
                    ],
                    Provenance::ModelOutput,
                )?,
                plakous_model::ConversationItem::new(
                    entry(3)?,
                    Actor::Assistant,
                    vec![ContentBlock::Opaque(OpaqueBlock::new(
                        future.clone(),
                        OpaqueFallback::CompatibleReplay,
                    )?)],
                    Provenance::ModelOutput,
                )?,
            ];
            let replay = vec![
                ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, 0, reasoning),
                ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, 1, message),
                ResponsesReplayItem::new(ResponsesReplayPurpose::OpaqueOutputItem, 2, future),
            ];
            Ok((output, replay))
        }

        /// One historical assistant refusal and its exact native replay.
        fn refusal_output(
            descriptor: &ResolvedModelDescriptor,
            round: u32,
            output_index: u32,
        ) -> TestResult<(plakous_model::ConversationItem, ResponsesReplayItem)> {
            let refusal = replay_envelope(
                descriptor,
                json!({
                    "id":format!("refusal_{round}"),
                    "type":"message",
                    "status":"completed",
                    "role":"assistant",
                    "content":[{"type":"refusal","refusal":format!("refused {round}")}]
                }),
                SUPERGROK_RESPONSES_DIALECT.output_item_replay(),
            )?;
            let item = plakous_model::ConversationItem::new(
                EntryId::from_str(&format!(
                    "00000000-0000-4000-8000-{:012}",
                    1_000 + round * 10 + 9
                ))?,
                Actor::Assistant,
                vec![
                    ContentBlock::Refusal(plakous_model::RefusalBlock::new(format!(
                        "refused {round}"
                    ))?),
                    ContentBlock::Opaque(OpaqueBlock::new(
                        refusal.clone(),
                        OpaqueFallback::CompatibleReplay,
                    )?),
                ],
                Provenance::ModelOutput,
            )?;
            Ok((
                item,
                ResponsesReplayItem::new(ResponsesReplayPurpose::OutputItem, output_index, refusal),
            ))
        }

        fn seal_round(
            descriptor: &ResolvedModelDescriptor,
            plan: &ValidatedRequestPlan,
            round: u32,
        ) -> TestResult<(ModelResponse, ResponsesArtifactBinding)> {
            let (output, replay) = canonical_output(descriptor, round)?;
            seal_output(descriptor, plan, round, output, replay)
        }

        fn seal_output(
            descriptor: &ResolvedModelDescriptor,
            plan: &ValidatedRequestPlan,
            round: u32,
            output: Vec<plakous_model::ConversationItem>,
            replay: Vec<ResponsesReplayItem>,
        ) -> TestResult<(ModelResponse, ResponsesArtifactBinding)> {
            let covered = covered_projection_after_output(plan, &output)?;
            let binding = authority(descriptor, plan, supergrok_semantic_revisions()?)?
                .artifact_binding()
                .clone();
            let round_bytes = round.to_be_bytes();
            let physical = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"xai-model-artifact-physical", &round_bytes],
            )?;
            let terminal = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"xai-model-artifact-terminal", &round_bytes],
            )?;
            let production = ResponsesArtifactProduction::from_admitted_response(
                plan.request(),
                plan,
                physical,
                terminal,
                u32::try_from(output.len())?,
                covered.identity(),
            )?;
            let contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
                ReplayKindId::new(SUPERGROK_RESPONSES_DIALECT.output_item_replay().name())?,
                SUPERGROK_RESPONSES_DIALECT.output_item_replay().version(),
            )?)
            .with_opaque_output(ResponsesArtifactReplayKind::new(
                ReplayKindId::new(SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().name())?,
                SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().version(),
            )?)?;
            let artifact = ResponsesArtifact::new(
                binding.clone(),
                production.clone(),
                contract,
                replay,
                None,
                None,
                ResponsesStorageMode::Stateless,
                None,
                now(),
            )?;
            let producer = FamilyArtifactProducerFacts::new(
                production.producer_request(),
                binding.model_input(),
                production.validated_plan(),
                binding.cache_affinity(),
                binding.run(),
                binding.root(),
                binding.attempt(),
                production.producing_projection(),
                production.producing_projection_items(),
                production.output_items(),
                production.covered_projection(),
                production.covered_projection_items(),
                production.physical_request(),
                production.terminal_response(),
            )?;
            let candidate = SealedFamilyArtifactCandidate::new(
                "responses",
                RESPONSES_ARTIFACT_SCHEMA_REVISION,
                RESPONSES_ARTIFACT_CODEC_REVISION,
                binding.identity(),
                producer,
                ResponsesArtifactCodec::encode(&artifact)?,
            )?;
            candidate.validate_for_commit(plan, covered, output.len())?;
            let response = ModelResponse::new(
                output,
                FinishDetails::new(FinishReason::Stop, None),
                None,
                Some(format!("resp_{round}")),
                plan.digest(),
            )
            .with_family_artifact_candidate(candidate)?;
            Ok((response, binding))
        }

        fn active(response: &ModelResponse) -> TestResult<ActiveFamilyArtifact> {
            let candidate = response
                .family_artifact_candidate()
                .ok_or_else(|| std::io::Error::other("missing family artifact candidate"))?;
            Ok(ActiveFamilyArtifact::new(
                candidate.family(),
                candidate.schema_revision(),
                candidate.codec_revision(),
                candidate.binding(),
                candidate.bytes().to_vec(),
            )?)
        }

        fn output_segment(response: &ModelResponse) -> TestResult<HistorySegment> {
            Ok(HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                response.output().to_vec(),
            )?)
        }

        fn assert_item_replay_without_aggregate(response: &ModelResponse) -> TestResult {
            let candidate = response
                .family_artifact_candidate()
                .ok_or_else(|| std::io::Error::other("missing family artifact candidate"))?;
            assert_eq!(candidate.family(), "responses");
            assert_eq!(
                candidate.schema_revision(),
                RESPONSES_ARTIFACT_SCHEMA_REVISION
            );
            assert_eq!(
                candidate.codec_revision(),
                RESPONSES_ARTIFACT_CODEC_REVISION
            );
            assert!(candidate.bytes().starts_with(b"PLAKRSP\0"));
            assert!(
                !serde_json::to_value(response)?
                    .as_object()
                    .is_some_and(|object| object.contains_key("replay")),
                "aggregate replay field is forbidden"
            );
            let kinds = response
                .output()
                .iter()
                .flat_map(plakous_model::ConversationItem::blocks)
                .filter_map(|block| match block {
                    ContentBlock::Opaque(opaque) => Some(opaque.replay().kind().as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(kinds.contains(&SUPERGROK_RESPONSES_DIALECT.output_item_replay().name()));
            assert!(kinds.contains(&SUPERGROK_RESPONSES_DIALECT.opaque_output_replay().name()));
            let canonical = serde_json::to_vec(response.output())?;
            assert!(!canonical.windows(7).any(|window| window == b"PLAKRSP"));
            assert!(
                !canonical
                    .windows(b"responses.artifact".len())
                    .any(|window| window == b"responses.artifact")
            );
            Ok(())
        }

        #[test]
        fn stateless_artifact_chains_one_two_three_and_reopens_context() -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let revisions = supergrok_semantic_revisions()?;
            let mut segments = vec![user_segment(1)?];
            let first_request = request(segments.clone(), 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let (first, first_binding) = seal_round(&descriptor, &first_plan, 1)?;
            assert_item_replay_without_aggregate(&first)?;
            segments.push(output_segment(&first)?);
            segments.push(user_segment(2)?);

            let second_request = request(segments.clone(), 2)?;
            let second_plan =
                descriptor.validate_request(second_request, ValidationMode::Strict)?;
            let second_authority = authority(&descriptor, &second_plan, revisions)?;
            assert_ne!(
                first_binding.identity(),
                second_authority.artifact_binding().identity()
            );
            let first_active = active(&first)?;
            let second_bridge = active_replay_bridge(
                Some(&first_active),
                second_plan.request(),
                second_authority,
                now(),
            )?
            .ok_or_else(|| std::io::Error::other("second round omitted replay bridge"))?;
            let second_wire = lower_responses_request(
                second_plan.request(),
                ResponsesRequestConfig::new(
                    descriptor.protocol(),
                    descriptor.model_family(),
                    descriptor.requested_model(),
                    SUPERGROK_RESPONSES_DIALECT.request_policy(),
                    Some(second_bridge.context()),
                ),
            )?;
            let second_wire = second_wire.body().to_string();
            assert!(second_wire.contains("reasoning_1"));
            assert!(second_wire.contains("encrypted-1"));
            assert!(second_wire.contains("future_1"));
            assert!(!second_wire.contains("PLAKRSP"));
            assert!(!second_wire.contains("responses.artifact"));

            let (second, second_binding) = seal_round(&descriptor, &second_plan, 2)?;
            assert_item_replay_without_aggregate(&second)?;
            let second_candidate = second
                .family_artifact_candidate()
                .ok_or_else(|| std::io::Error::other("missing second candidate"))?;
            assert_eq!(second_candidate.binding(), second_binding.identity());
            segments.push(output_segment(&second)?);
            segments.push(user_segment(3)?);

            let third_request = request(segments, 3)?;
            let third_plan = descriptor.validate_request(third_request, ValidationMode::Strict)?;
            let third_authority = authority(&descriptor, &third_plan, revisions)?;
            assert_ne!(
                second_binding.identity(),
                third_authority.artifact_binding().identity()
            );
            let serialized = serde_json::to_vec(&active(&second)?)?;
            let reopened: ActiveFamilyArtifact = serde_json::from_slice(&serialized)?;
            let third_bridge = active_replay_bridge(
                Some(&reopened),
                third_plan.request(),
                third_authority,
                now(),
            )?
            .ok_or_else(|| std::io::Error::other("reopened third round omitted replay bridge"))?;
            let third_wire = lower_responses_request(
                third_plan.request(),
                ResponsesRequestConfig::new(
                    descriptor.protocol(),
                    descriptor.model_family(),
                    descriptor.requested_model(),
                    SUPERGROK_RESPONSES_DIALECT.request_policy(),
                    Some(third_bridge.context()),
                ),
            )?
            .body()
            .to_string();
            assert!(third_wire.contains("reasoning_1"));
            assert!(third_wire.contains("reasoning_2"));
            assert!(third_wire.contains("encrypted-2"));
            assert!(third_wire.contains("future_2"));
            assert!(!third_wire.contains("PLAKRSP"));

            let (third, third_binding) = seal_round(&descriptor, &third_plan, 3)?;
            assert_item_replay_without_aggregate(&third)?;
            assert_eq!(
                third
                    .family_artifact_candidate()
                    .ok_or_else(|| std::io::Error::other("missing third candidate"))?
                    .binding(),
                third_binding.identity()
            );
            Ok(())
        }

        /// A compaction beside a live artifact prepares fresh instead of failing.
        ///
        /// An active artifact continues one exact canonical prefix. A
        /// compaction fold covers a sealed cut, which is a different prefix, so
        /// the artifact is simply not this request's. Refusing the whole
        /// preparation is what killed every compaction the live Grok and Codex
        /// growth turns attempted: "Responses artifact canonical coverage
        /// differs from the current request", with the fold rung dead for the
        /// rest of the session.
        #[test]
        fn a_compaction_request_prepares_fresh_beside_a_live_artifact() -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let first_request = request(vec![user_segment(1)?], 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let (first, _) = seal_round(&descriptor, &first_plan, 1)?;
            let first_active = active(&first)?;

            // The compaction's own request: a different canonical prefix.
            let fold_request = request(vec![user_segment(2)?], 2)?;
            let fold_plan = descriptor.validate_request(fold_request, ValidationMode::Strict)?;
            let fold_authority =
                authority(&descriptor, &fold_plan, supergrok_semantic_revisions()?)?;
            let fold_bridge = active_replay_bridge(
                Some(&first_active),
                fold_plan.request(),
                fold_authority,
                now(),
            )?;
            assert!(
                fold_bridge.is_none(),
                "the compaction must prepare fresh, not refuse"
            );
            // And the wire it lowers to continues nothing: a sealed cut carries
            // its own coverage and asks the route to replay none of the live
            // turn's.
            let fold_wire = lower_responses_request(
                fold_plan.request(),
                ResponsesRequestConfig::new(
                    descriptor.protocol(),
                    descriptor.model_family(),
                    descriptor.requested_model(),
                    SUPERGROK_RESPONSES_DIALECT.request_policy(),
                    fold_bridge
                        .as_ref()
                        .map(ResponsesArtifactReplayBridge::context),
                ),
            )?;
            assert!(
                fold_wire
                    .body()
                    .to_string()
                    .find("previous_response_id")
                    .is_none(),
                "a compaction request must not continue a previous response"
            );

            // And the main turn still reuses its own artifact afterwards.
            let second_request = request(
                vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?],
                2,
            )?;
            let second_plan =
                descriptor.validate_request(second_request, ValidationMode::Strict)?;
            let second_authority =
                authority(&descriptor, &second_plan, supergrok_semantic_revisions()?)?;
            assert!(
                active_replay_bridge(
                    Some(&first_active),
                    second_plan.request(),
                    second_authority,
                    now(),
                )?
                .is_some(),
                "the live turn's own continuation still reuses its artifact"
            );
            Ok(())
        }

        /// The operation-budget wrap-up round keeps the live artifact's replay.
        ///
        /// The wrap-up keeps the live history and prefix but withdraws tool
        /// selection, which the artifact binding leaves out of its prefix
        /// identity. Its history holds a retained unknown output item and an
        /// assistant refusal, which fresh lowering refuses as
        /// `ProtectedHistory`: preparing it fresh once failed the whole turn.
        /// The next turn's first round, back on ordinary tool selection,
        /// continues the artifact the wrap-up produced.
        #[test]
        fn a_tool_free_wrap_up_round_keeps_bound_replay_and_the_next_turn_binds_it() -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let revisions = supergrok_semantic_revisions()?;
            let lower = |request: &ModelRequest, bridge: Option<&ResponsesArtifactReplayBridge>| {
                lower_responses_request(
                    request,
                    ResponsesRequestConfig::new(
                        descriptor.protocol(),
                        descriptor.model_family(),
                        descriptor.requested_model(),
                        SUPERGROK_RESPONSES_DIALECT.request_policy(),
                        bridge.map(ResponsesArtifactReplayBridge::context),
                    ),
                )
            };
            let first_request = request(vec![user_segment(1)?], 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let (mut output, mut replay) = canonical_output(&descriptor, 1)?;
            let (refusal, refusal_replay) = refusal_output(&descriptor, 1, 3)?;
            output.push(refusal);
            replay.push(refusal_replay);
            let (first, _) = seal_output(&descriptor, &first_plan, 1, output, replay)?;
            let first_active = active(&first)?;
            let segments = vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?];

            let wrap_up_plan = descriptor.validate_request(
                tool_free_request(segments.clone(), 2)?,
                ValidationMode::Strict,
            )?;
            let wrap_up_bridge = active_replay_bridge(
                Some(&first_active),
                wrap_up_plan.request(),
                authority(&descriptor, &wrap_up_plan, revisions)?,
                now(),
            )?
            .ok_or_else(|| std::io::Error::other("the wrap-up dropped the live artifact"))?;
            let wire = lower(wrap_up_plan.request(), Some(&wrap_up_bridge))?
                .body()
                .to_string();
            for exact in [
                "encrypted-1",
                "future_1",
                "opaque-1",
                "refusal_1",
                "refused 1",
            ] {
                assert!(wire.contains(exact), "wrap-up wire lost {exact}");
            }
            assert_eq!(
                lower(wrap_up_plan.request(), None)
                    .map_err(
                        |error| ResponsesAttemptBuildError::Request(error).preparation_error_kind()
                    )
                    .err(),
                Some(ModelPreparationErrorKind::ProtectedHistory),
                "fresh lowering of this history must stay refused"
            );

            // The wrap-up's own artifact is what the next turn continues.
            let (wrap_up, _) = seal_round(&descriptor, &wrap_up_plan, 2)?;
            let wrap_up_active = active(&wrap_up)?;
            let mut next_segments = segments;
            next_segments.push(output_segment(&wrap_up)?);
            next_segments.push(user_segment(3)?);
            let next_plan =
                descriptor.validate_request(request(next_segments, 3)?, ValidationMode::Strict)?;
            let next_authority = authority(&descriptor, &next_plan, revisions)?;
            assert!(
                active_replay_bridge(
                    Some(&first_active),
                    next_plan.request(),
                    next_authority.clone(),
                    now(),
                )?
                .is_none(),
                "the next turn does not continue the superseded artifact"
            );
            let next_bridge = active_replay_bridge(
                Some(&wrap_up_active),
                next_plan.request(),
                next_authority,
                now(),
            )?
            .ok_or_else(|| std::io::Error::other("the next turn dropped the wrap-up artifact"))?;
            let next_wire = lower(next_plan.request(), Some(&next_bridge))?
                .body()
                .to_string();
            for exact in ["encrypted-1", "refused 1", "encrypted-2", "future_2"] {
                assert!(next_wire.contains(exact), "next-turn wire lost {exact}");
            }
            Ok(())
        }

        /// Any stable-prefix difference other than tool selection prepares fresh.
        #[test]
        fn a_round_with_other_prefix_drift_prepares_fresh_beside_a_live_artifact() -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let revisions = supergrok_semantic_revisions()?;
            let first_request = request(vec![user_segment(1)?], 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let (first, _) = seal_round(&descriptor, &first_plan, 1)?;
            let first_active = active(&first)?;
            let segments = vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?];

            let (selected, effective) = provider_configuration_ids()?;
            let drifted_request = ModelRequest::builder(
                selected,
                effective,
                ConversationView::from_shared(
                    CanonicalHistory::new(segments.clone())?,
                    WorkingProjection::default(),
                ),
                metadata(2)?,
            )
            .instructions(vec![plakous_model::InstructionBlock::new(
                plakous_model::InstructionSource::System,
                "answer briefly",
            )?])
            .resolved_harness_plan_identity(harness_plan()?)
            .tool_choice(plakous_model::ToolChoice::None)
            .build()?;
            let drifted_plan =
                descriptor.validate_request(drifted_request, ValidationMode::Strict)?;
            assert!(
                active_replay_bridge(
                    Some(&first_active),
                    drifted_plan.request(),
                    authority(&descriptor, &drifted_plan, revisions)?,
                    now(),
                )?
                .is_none(),
                "an instruction drift must prepare fresh, not refuse"
            );

            // The same history on the live prefix still reuses the artifact:
            // only the instructions made the drifted round a miss.
            let next_plan =
                descriptor.validate_request(request(segments, 2)?, ValidationMode::Strict)?;
            assert!(
                active_replay_bridge(
                    Some(&first_active),
                    next_plan.request(),
                    authority(&descriptor, &next_plan, revisions)?,
                    now(),
                )?
                .is_some()
            );
            Ok(())
        }

        #[test]
        fn active_artifact_refuses_tamper_binding_revision_family_and_missing_authority()
        -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let revisions = supergrok_semantic_revisions()?;
            let first_request = request(vec![user_segment(1)?], 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let (first, producing_binding) = seal_round(&descriptor, &first_plan, 1)?;
            let candidate = first
                .family_artifact_candidate()
                .ok_or_else(|| std::io::Error::other("missing first candidate"))?;
            let second_request = request(
                vec![user_segment(1)?, output_segment(&first)?, user_segment(2)?],
                2,
            )?;
            let second_plan =
                descriptor.validate_request(second_request, ValidationMode::Strict)?;
            let second_authority = authority(&descriptor, &second_plan, revisions)?;
            assert_eq!(candidate.binding(), producing_binding.identity());
            assert_ne!(
                candidate.binding(),
                second_authority.artifact_binding().identity()
            );
            assert!(
                active_replay_bridge(
                    None,
                    second_plan.request(),
                    second_authority.clone(),
                    now(),
                )?
                .is_none()
            );

            let invalid = |artifact: ActiveFamilyArtifact| {
                active_replay_bridge(
                    Some(&artifact),
                    second_plan.request(),
                    second_authority.clone(),
                    now(),
                )
                .map(|bridge| bridge.is_some())
                .map_err(|error| -> Box<dyn Error> { Box::new(error) })
            };

            let mut tampered = candidate.bytes().to_vec();
            *tampered
                .last_mut()
                .ok_or_else(|| std::io::Error::other("empty candidate bytes"))? ^= 1;
            assert!(
                invalid(ActiveFamilyArtifact::new(
                    "responses",
                    candidate.schema_revision(),
                    candidate.codec_revision(),
                    candidate.binding(),
                    tampered,
                )?)
                .is_err()
            );
            let wrong_binding = Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"xai-model-artifact-wrong-wrapper-binding"],
            )?;
            assert!(
                invalid(ActiveFamilyArtifact::new(
                    "responses",
                    candidate.schema_revision(),
                    candidate.codec_revision(),
                    wrong_binding,
                    candidate.bytes().to_vec(),
                )?)
                .is_err()
            );
            assert!(
                invalid(ActiveFamilyArtifact::new(
                    "responses",
                    candidate.schema_revision() + 1,
                    candidate.codec_revision(),
                    candidate.binding(),
                    candidate.bytes().to_vec(),
                )?)
                .is_err()
            );
            assert!(
                invalid(ActiveFamilyArtifact::new(
                    "messages",
                    candidate.schema_revision(),
                    candidate.codec_revision(),
                    candidate.binding(),
                    candidate.bytes().to_vec(),
                )?)
                .is_err()
            );

            let revision_authority = authority(
                &descriptor,
                &second_plan,
                ResponsesSemanticRevisions::new(4, 1, 5)?,
            )?;
            let revision_active = active(&first)?;
            assert!(
                active_replay_bridge(
                    Some(&revision_active),
                    second_plan.request(),
                    revision_authority,
                    now(),
                )
                .is_err()
            );
            let foreign_descriptor = test_descriptor("grok-foreign", "xai-test-route")?;
            let foreign_plan = foreign_descriptor
                .validate_request(second_plan.request().clone(), ValidationMode::Strict)?;
            let foreign_active = active(&first)?;
            assert!(
                active_replay_bridge(
                    Some(&foreign_active),
                    foreign_plan.request(),
                    authority(&foreign_descriptor, &foreign_plan, revisions)?,
                    now(),
                )
                .is_err()
            );
            Ok(())
        }

        #[test]
        fn tool_output_coverage_is_the_open_prefix_of_the_complete_exchange() -> TestResult {
            let descriptor = test_descriptor(MODEL, "xai-test-route")?;
            let intent = user_segment(1)?;
            let first_request = request(vec![intent.clone()], 1)?;
            let first_plan = descriptor.validate_request(first_request, ValidationMode::Strict)?;
            let correlation = ToolCorrelation::new(
                Some("remote-call-1".to_owned()),
                None,
                plakous_model::ToolCallerLineage::Direct,
            )?;
            let call_id = plakous_model::CallId::from_str("00000000-0000-4000-8000-000000000901")?;
            let call = ToolCallBlock::new(
                call_id,
                ToolName::new("fixture_tool")?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                correlation.clone(),
                r#"{"path":"fixture.txt"}"#,
            )?;
            let call_item = plakous_model::ConversationItem::new(
                EntryId::from_str("00000000-0000-4000-8000-000000000902")?,
                Actor::Assistant,
                vec![ContentBlock::ToolCall(call)],
                Provenance::ModelOutput,
            )?;
            let covered =
                covered_projection_after_output(&first_plan, std::slice::from_ref(&call_item))?;
            let result = ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new(
                    "fixture result",
                    None,
                )?)],
                correlation,
            )?;
            let result_item = plakous_model::ConversationItem::new(
                EntryId::from_str("00000000-0000-4000-8000-000000000903")?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(result)],
                Provenance::ToolExecution,
            )?;
            let exchange = HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![call_item, result_item],
            )?;
            let next = ConversationView::from_shared(
                CanonicalHistory::new(vec![intent, exchange])?,
                WorkingProjection::default(),
            );
            assert_eq!(covered, next.canonical_item_prefix_identity(2)?);
            Ok(())
        }
    }
}

#[cfg(feature = "native-auth")]
pub use shared_host_model::{StandardXaiHttpConfig, XaiLanguageModel, XaiLanguageModelConfigError};
