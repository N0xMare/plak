use super::{
    ActiveFamilyArtifact, AttemptId, Deserialize, Deserializer, Digest, DigestDomain,
    EffectiveProviderConfigurationId, ModelAdapterProfileId, ModelCacheAffinity, ModelId,
    ModelOperationRoot, ModelPreparationContext, PreparedCacheUse, PreparedCallError,
    PreparedModelProjection, PreparedModelProjectionContent, PreparedModelProjectionWire,
    PreparedOutputLimit, PreparedOutputLimitSource, PreparedTokenProjection, ProtocolId,
    ResolvedModelDescriptor, RouteId, SelectedProviderConfigurationId, ValidatedRequestPlan,
    prepared_output_scope_identity, prepared_output_scope_identity_from_axes,
    validate_preparation_label, validate_prepared_digest, validate_prepared_wire,
};

impl PreparedModelProjection {
    /// Constructs and seals the exact non-secret result of one lowering.
    ///
    /// The descriptor, admitted plan, fresh authority, fixed artifact, exact
    /// wire body, count projection, cache use, and output proof are checked as
    /// one trust-boundary result. No attempt/cancellation/deadline fact enters
    /// the lowering identity.
    ///
    /// # Errors
    ///
    /// Rejects any scope drift, missing integrity, invalid label, or oversized wire.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::similar_names,
        reason = "the complete exact prepared projection is checked as one auditable trust-boundary operation"
    )]
    pub fn new(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        protocol_family: ProtocolId,
        lowering_revision: impl Into<String>,
        projection_revision: impl Into<String>,
        accounting_revision: impl Into<String>,
        wire_digest: Digest,
        wire_size: u64,
        token_projection: PreparedTokenProjection,
        cache_use: PreparedCacheUse,
        output_limit: Option<PreparedOutputLimit>,
    ) -> Result<Self, PreparedCallError> {
        descriptor
            .validate_plan_binding(plan)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        context
            .authority
            .validate(descriptor, context.prepared_at)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "authority" })?;
        validate_prepared_wire(wire_digest, wire_size)?;
        if token_projection.wire_digest != wire_digest || token_projection.wire_size != wire_size {
            return Err(PreparedCallError::ScopeDrift {
                field: "token projection",
            });
        }
        let lowering_revision = lowering_revision.into();
        let projection_revision = projection_revision.into();
        let accounting_revision = accounting_revision.into();
        validate_preparation_label(&lowering_revision)?;
        validate_preparation_label(&projection_revision)?;
        validate_preparation_label(&accounting_revision)?;
        let active_artifact = context
            .active_family_artifact
            .as_ref()
            .map(ActiveFamilyArtifact::integrity);
        let cache_affinity = context
            .cache_affinity
            .as_ref()
            .map(ModelCacheAffinity::identity);
        match cache_use {
            PreparedCacheUse::None => {}
            PreparedCacheUse::ActiveFamilyArtifact { artifact }
                if active_artifact == Some(artifact) => {}
            PreparedCacheUse::RuntimeAffinity { affinity } if cache_affinity == Some(affinity) => {}
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity }
                if active_artifact == Some(artifact) && cache_affinity == Some(affinity) => {}
            PreparedCacheUse::ActiveFamilyArtifact { .. }
            | PreparedCacheUse::RuntimeAffinity { .. }
            | PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { .. } => {
                return Err(PreparedCallError::ScopeDrift { field: "cache use" });
            }
        }
        if let Some(limit) = &output_limit {
            match limit.source() {
                PreparedOutputLimitSource::EnforcedWireMaximum {
                    wire_digest: proof_wire,
                    lowering_revision: proof_lowering,
                    ..
                } if *proof_wire == wire_digest && *proof_lowering == lowering_revision => {}
                PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. }
                    if *scope
                        == prepared_output_scope_identity(
                            descriptor,
                            context,
                            &protocol_family,
                            &lowering_revision,
                            &projection_revision,
                            &accounting_revision,
                        )? => {}
                _ => {
                    return Err(PreparedCallError::ScopeDrift {
                        field: "output limit",
                    });
                }
            }
        }
        let authority = context.authority.execution_authority_binding()?;
        let authority_class = context.authority.authority_class_binding()?;
        let request = plan.request();
        let effective_model = descriptor
            .reported_model()
            .unwrap_or(descriptor.requested_model())
            .clone();
        let content = PreparedModelProjectionContent {
            plan: plan.digest(),
            selected_provider_configuration: request.selected_provider_configuration_id(),
            effective_provider_configuration: request.effective_provider_configuration_id(),
            descriptor_binding: plan.binding_digest(),
            request: request.identity(),
            model_input: request.model_input_identity(),
            operation_root: request.metadata().root(),
            attempt_id: request.metadata().attempt_id(),
            protocol_family: &protocol_family,
            dialect: descriptor.protocol(),
            route: descriptor.route(),
            authority,
            authority_class,
            requested_model: descriptor.requested_model(),
            effective_model: &effective_model,
            adapter: descriptor.profile(),
            lowering_revision: &lowering_revision,
            projection_revision: &projection_revision,
            accounting_revision: &accounting_revision,
            wire_digest,
            wire_size,
            token_projection: token_projection.identity,
            active_artifact,
            cache_affinity,
            cache_use,
            output_limit: output_limit.as_ref().map(PreparedOutputLimit::identity),
        };
        let identity = prepared_projection_identity(&content)?;
        Ok(Self {
            plan: content.plan,
            selected_provider_configuration: content.selected_provider_configuration,
            effective_provider_configuration: content.effective_provider_configuration,
            descriptor_binding: content.descriptor_binding,
            request: content.request,
            model_input: content.model_input,
            operation_root: content.operation_root,
            attempt_id: content.attempt_id,
            protocol_family,
            dialect: descriptor.protocol().clone(),
            route: descriptor.route().clone(),
            authority,
            authority_class,
            requested_model: descriptor.requested_model().clone(),
            effective_model,
            adapter: descriptor.profile().clone(),
            lowering_revision,
            projection_revision,
            accounting_revision,
            wire_digest,
            wire_size,
            token_projection,
            active_artifact,
            cache_affinity,
            cache_use,
            output_limit,
            identity,
        })
    }

    /// Validates every neutral scope axis against the exact plan and descriptor.
    ///
    /// # Errors
    ///
    /// Returns the first exact scope dimension which drifted.
    pub fn validate_for_plan(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), PreparedCallError> {
        descriptor
            .validate_plan_binding(plan)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        self.validate_plan_axes(descriptor, plan)
    }

    /// Checks every projection axis against `plan` and the semantic axes of
    /// `descriptor`, but not that the plan is bound to this exact observation
    /// of the descriptor.
    #[allow(
        clippy::too_many_lines,
        reason = "all prepared scope axes are rechecked together at the trust boundary"
    )]
    pub(in super::super) fn validate_plan_axes(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), PreparedCallError> {
        let request = plan.request();
        let effective_model = descriptor
            .reported_model()
            .unwrap_or(descriptor.requested_model());
        let content = PreparedModelProjectionContent {
            plan: self.plan,
            selected_provider_configuration: self.selected_provider_configuration,
            effective_provider_configuration: self.effective_provider_configuration,
            descriptor_binding: self.descriptor_binding,
            request: self.request,
            model_input: self.model_input,
            operation_root: self.operation_root,
            attempt_id: self.attempt_id,
            protocol_family: &self.protocol_family,
            dialect: &self.dialect,
            route: &self.route,
            authority: self.authority,
            authority_class: self.authority_class,
            requested_model: &self.requested_model,
            effective_model: &self.effective_model,
            adapter: &self.adapter,
            lowering_revision: &self.lowering_revision,
            projection_revision: &self.projection_revision,
            accounting_revision: &self.accounting_revision,
            wire_digest: self.wire_digest,
            wire_size: self.wire_size,
            token_projection: self.token_projection.identity,
            active_artifact: self.active_artifact,
            cache_affinity: self.cache_affinity,
            cache_use: self.cache_use,
            output_limit: self
                .output_limit
                .as_ref()
                .map(PreparedOutputLimit::identity),
        };
        let output_scope = prepared_output_scope_identity_from_axes(
            self.descriptor_binding,
            &self.protocol_family,
            &self.dialect,
            &self.route,
            self.authority_class,
            &self.requested_model,
            &self.effective_model,
            &self.adapter,
            &self.lowering_revision,
            &self.projection_revision,
            &self.accounting_revision,
        )?;
        let cache_use_matches = match self.cache_use {
            PreparedCacheUse::None => true,
            PreparedCacheUse::ActiveFamilyArtifact { artifact } => {
                self.active_artifact == Some(artifact)
            }
            PreparedCacheUse::RuntimeAffinity { affinity } => self.cache_affinity == Some(affinity),
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity } => {
                self.active_artifact == Some(artifact) && self.cache_affinity == Some(affinity)
            }
        };
        let checks = [
            (self.plan == plan.digest(), "plan"),
            (
                self.selected_provider_configuration
                    == request.selected_provider_configuration_id(),
                "selected provider configuration",
            ),
            (
                self.effective_provider_configuration
                    == request.effective_provider_configuration_id(),
                "effective provider configuration",
            ),
            (
                self.descriptor_binding == plan.binding_digest(),
                "descriptor binding",
            ),
            (self.request == request.identity(), "request"),
            (
                self.model_input == request.model_input_identity(),
                "model input",
            ),
            (
                self.operation_root == request.metadata().root(),
                "operation root",
            ),
            (
                self.attempt_id == request.metadata().attempt_id(),
                "attempt",
            ),
            (self.dialect == *descriptor.protocol(), "dialect"),
            (self.route == *descriptor.route(), "route"),
            (
                self.requested_model == *descriptor.requested_model(),
                "requested model",
            ),
            (self.effective_model == *effective_model, "effective model"),
            (self.adapter == *descriptor.profile(), "adapter"),
            (
                self.token_projection.wire_digest == self.wire_digest
                    && self.token_projection.wire_size == self.wire_size,
                "token projection",
            ),
            (cache_use_matches, "cache use"),
            (
                self.output_limit
                    .as_ref()
                    .is_none_or(|limit| match limit.source() {
                        PreparedOutputLimitSource::EnforcedWireMaximum {
                            wire_digest,
                            lowering_revision,
                            ..
                        } => {
                            *wire_digest == self.wire_digest
                                && *lowering_revision == self.lowering_revision
                        }
                        PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. } => {
                            *scope == output_scope
                        }
                    }),
                "output limit",
            ),
            (
                prepared_projection_identity(&content)? == self.identity,
                "identity",
            ),
        ];
        for (valid, field) in checks {
            if !valid {
                return Err(PreparedCallError::ScopeDrift { field });
            }
        }
        Ok(())
    }

    /// Returns the admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }
    /// Returns the caller-selected provider configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }
    /// Returns the compiled provider configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }
    /// Returns the complete resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }
    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }
    /// Returns the complete model-input identity.
    #[must_use]
    pub const fn model_input(&self) -> Digest {
        self.model_input
    }
    /// Returns the exact logical operation root.
    #[must_use]
    pub const fn operation_root(&self) -> ModelOperationRoot {
        self.operation_root
    }
    /// Returns the logical attempt fixed by the request.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }
    /// Returns the protocol-family identity.
    #[must_use]
    pub const fn protocol_family(&self) -> &ProtocolId {
        &self.protocol_family
    }
    /// Returns the exact dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> &ProtocolId {
        &self.dialect
    }
    /// Returns the exact route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }
    /// Returns the complete non-secret fresh-authority identity.
    #[must_use]
    pub const fn authority(&self) -> Digest {
        self.authority
    }
    /// Returns the stable account/credential authority class.
    #[must_use]
    pub const fn authority_class(&self) -> Digest {
        self.authority_class
    }
    /// Returns the requested model.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }
    /// Returns the effective model.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }
    /// Returns the adapter identity.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }
    /// Returns the lowering behavior revision.
    #[must_use]
    pub fn lowering_revision(&self) -> &str {
        &self.lowering_revision
    }
    /// Returns the family projection revision.
    #[must_use]
    pub fn projection_revision(&self) -> &str {
        &self.projection_revision
    }
    /// Returns the context accounting revision.
    #[must_use]
    pub fn accounting_revision(&self) -> &str {
        &self.accounting_revision
    }
    /// Returns the exact non-secret wire digest.
    #[must_use]
    pub const fn wire_digest(&self) -> Digest {
        self.wire_digest
    }
    /// Returns the exact non-secret wire size.
    #[must_use]
    pub const fn wire_size(&self) -> u64 {
        self.wire_size
    }
    /// Returns the complete prepared token projection.
    #[must_use]
    pub const fn token_projection(&self) -> &PreparedTokenProjection {
        &self.token_projection
    }
    /// Returns the fixed active-artifact identity.
    #[must_use]
    pub const fn active_artifact(&self) -> Option<Digest> {
        self.active_artifact
    }
    /// Returns the fixed redacted runtime cache-affinity identity.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.cache_affinity
    }
    /// Returns the exact cache behavior.
    #[must_use]
    pub const fn cache_use(&self) -> PreparedCacheUse {
        self.cache_use
    }
    /// Returns the authoritative prepared output limit, when present.
    #[must_use]
    pub const fn output_limit(&self) -> Option<&PreparedOutputLimit> {
        self.output_limit.as_ref()
    }
    /// Returns the complete prepared-projection identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedModelProjection {
    #[allow(
        clippy::too_many_lines,
        reason = "deserialization rechecks every retained projection invariant before construction"
    )]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedModelProjectionWire::deserialize(deserializer)?;
        let value = Self {
            plan: wire.plan,
            selected_provider_configuration: wire.selected_provider_configuration,
            effective_provider_configuration: wire.effective_provider_configuration,
            descriptor_binding: wire.descriptor_binding,
            request: wire.request,
            model_input: wire.model_input,
            operation_root: wire.operation_root,
            attempt_id: wire.attempt_id,
            protocol_family: wire.protocol_family,
            dialect: wire.dialect,
            route: wire.route,
            authority: wire.authority,
            authority_class: wire.authority_class,
            requested_model: wire.requested_model,
            effective_model: wire.effective_model,
            adapter: wire.adapter,
            lowering_revision: wire.lowering_revision,
            projection_revision: wire.projection_revision,
            accounting_revision: wire.accounting_revision,
            wire_digest: wire.wire_digest,
            wire_size: wire.wire_size,
            token_projection: wire.token_projection,
            active_artifact: wire.active_artifact,
            cache_affinity: wire.cache_affinity,
            cache_use: wire.cache_use,
            output_limit: wire.output_limit,
            identity: wire.identity,
        };
        validate_prepared_wire(value.wire_digest, value.wire_size)
            .map_err(serde::de::Error::custom)?;
        validate_prepared_digest(value.plan, DigestDomain::RequestPlan)
            .and_then(|()| {
                validate_prepared_digest(value.descriptor_binding, DigestDomain::ModelBinding)
            })
            .and_then(|()| validate_prepared_digest(value.request, DigestDomain::RequestPlan))
            .and_then(|()| validate_prepared_digest(value.model_input, DigestDomain::RequestPlan))
            .and_then(|()| validate_prepared_digest(value.authority, DigestDomain::ModelBinding))
            .and_then(|()| {
                validate_prepared_digest(value.authority_class, DigestDomain::ModelBinding)
            })
            .and_then(|()| {
                value.active_artifact.map_or(Ok(()), |digest| {
                    validate_prepared_digest(digest, DigestDomain::Blob)
                })
            })
            .and_then(|()| {
                value.cache_affinity.map_or(Ok(()), |digest| {
                    validate_prepared_digest(digest, DigestDomain::CacheAffinity)
                })
            })
            .map_err(serde::de::Error::custom)?;
        validate_preparation_label(&value.lowering_revision)
            .and_then(|()| validate_preparation_label(&value.projection_revision))
            .and_then(|()| validate_preparation_label(&value.accounting_revision))
            .map_err(serde::de::Error::custom)?;
        let content = PreparedModelProjectionContent {
            plan: value.plan,
            selected_provider_configuration: value.selected_provider_configuration,
            effective_provider_configuration: value.effective_provider_configuration,
            descriptor_binding: value.descriptor_binding,
            request: value.request,
            model_input: value.model_input,
            operation_root: value.operation_root,
            attempt_id: value.attempt_id,
            protocol_family: &value.protocol_family,
            dialect: &value.dialect,
            route: &value.route,
            authority: value.authority,
            authority_class: value.authority_class,
            requested_model: &value.requested_model,
            effective_model: &value.effective_model,
            adapter: &value.adapter,
            lowering_revision: &value.lowering_revision,
            projection_revision: &value.projection_revision,
            accounting_revision: &value.accounting_revision,
            wire_digest: value.wire_digest,
            wire_size: value.wire_size,
            token_projection: value.token_projection.identity,
            active_artifact: value.active_artifact,
            cache_affinity: value.cache_affinity,
            cache_use: value.cache_use,
            output_limit: value
                .output_limit
                .as_ref()
                .map(PreparedOutputLimit::identity),
        };
        let cache_use_matches = match value.cache_use {
            PreparedCacheUse::None => true,
            PreparedCacheUse::ActiveFamilyArtifact { artifact } => {
                value.active_artifact == Some(artifact)
            }
            PreparedCacheUse::RuntimeAffinity { affinity } => {
                value.cache_affinity == Some(affinity)
            }
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity { artifact, affinity } => {
                value.active_artifact == Some(artifact) && value.cache_affinity == Some(affinity)
            }
        };
        if !cache_use_matches {
            return Err(serde::de::Error::custom(PreparedCallError::ScopeDrift {
                field: "cache use",
            }));
        }
        let output_scope = prepared_output_scope_identity_from_axes(
            value.descriptor_binding,
            &value.protocol_family,
            &value.dialect,
            &value.route,
            value.authority_class,
            &value.requested_model,
            &value.effective_model,
            &value.adapter,
            &value.lowering_revision,
            &value.projection_revision,
            &value.accounting_revision,
        )
        .map_err(serde::de::Error::custom)?;
        let output_limit_matches =
            value
                .output_limit
                .as_ref()
                .is_none_or(|limit| match limit.source() {
                    PreparedOutputLimitSource::EnforcedWireMaximum {
                        wire_digest,
                        lowering_revision,
                        ..
                    } => {
                        *wire_digest == value.wire_digest
                            && *lowering_revision == value.lowering_revision
                    }
                    PreparedOutputLimitSource::ReleaseVerifiedHardCap { scope, .. } => {
                        *scope == output_scope
                    }
                });
        if !output_limit_matches {
            return Err(serde::de::Error::custom(PreparedCallError::ScopeDrift {
                field: "output limit",
            }));
        }
        if prepared_projection_identity(&content).map_err(serde::de::Error::custom)?
            != value.identity
        {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

fn prepared_projection_identity(
    content: &PreparedModelProjectionContent<'_>,
) -> Result<Digest, PreparedCallError> {
    let bytes = serde_json::to_vec(content).map_err(|_| PreparedCallError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ContextBudget,
        11,
        &[b"plakous-model/prepared-model-projection-v2", &bytes],
    )?)
}
