use super::{
    Digest, EffectiveProviderConfigurationId, FeedSeq, ForkSessionOutcome, ForkSessionRequest,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId, RebindSessionOutcome,
    RebindSessionRequest, RebindToolTransition, RouteId, SelectedProviderConfigurationId,
    SessionBindingReport, SessionLocator, SessionOperationId, SessionRuntimeBinding,
    SessionTargetBinding, Timestamp, fmt,
};

impl ForkSessionRequest {
    /// Creates one exact source-preserving fork request.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
            runtime: None,
        }
    }

    /// Creates a fork that keeps the source's history and binding and
    /// recompiles the rest from `runtime`, which must be freshly admitted for
    /// the source's own target: the target plan carries the caller's current
    /// instruction sources. An unchanged compilation publishes an exact
    /// ordinary fork; a changed one publishes a configuration transition onto
    /// the same target. Recover it with the same constructor. A runtime for
    /// another target is refused; that transition is a rebind.
    #[must_use]
    pub const fn recompiling(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            runtime: Some(runtime),
        }
    }
}

impl fmt::Debug for ForkSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForkSessionRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("recompiling", &self.runtime.is_some())
            .finish()
    }
}

impl SessionTargetBinding {
    /// Creates one explicit target selection. No current default is consulted.
    #[must_use]
    pub const fn new(
        protocol: ProtocolId,
        route: RouteId,
        model: ModelId,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        account_binding: Digest,
    ) -> Self {
        Self {
            protocol,
            route,
            model,
            selected_provider_configuration,
            effective_provider_configuration,
            account_binding,
        }
    }

    /// Returns the target protocol/dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the target route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the target model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the exact caller-selected provider configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }

    /// Returns the exact compiled provider behavior identity.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }

    /// Returns the non-secret exact target account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }
}

impl RebindSessionRequest {
    /// Creates one exact confirmed target transition with fresh sealed
    /// authority; it keeps the source's presentation and instruction sources.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition: RebindToolTransition::PreserveExact,
            new_line: false,
        }
    }

    /// Creates a rebind that recompiles for its target: the qualified target
    /// presentation and the caller's current instruction sources.
    #[must_use]
    pub const fn recompiling_target(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition: RebindToolTransition::RecompileForTarget,
            new_line: false,
        }
    }

    /// Publishes the target as the first branch of a new agent line: a fork
    /// onto the new binding. The source keeps its line, stays writable and
    /// keeps its processes; the target continues only their numbering.
    /// Without this a rebind advances the source's line: the target becomes
    /// its next head, takes its jobs, and the source becomes read-only.
    #[must_use]
    pub const fn starting_line(mut self) -> Self {
        self.new_line = true;
        self
    }
}

impl fmt::Debug for RebindSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RebindSessionRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("target", &self.target)
            .field("runtime", &self.runtime)
            .field("tool_transition", &self.tool_transition)
            .field("new_line", &self.new_line)
            .finish()
    }
}

impl SessionBindingReport {
    /// Returns the exact historical protocol/dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }
    /// Returns the exact historical route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }
    /// Returns the exact historical requested model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }
    /// Returns the provider-reported model identity, when recorded.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }
    /// Returns the resolved model-family identity, when recorded.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }
    /// Returns the exact selected model-adapter profile.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }
    /// Returns the complete historical model-binding identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.binding
    }
    /// Returns the exact complete caller-selected provider configuration.
    #[must_use]
    pub const fn selected_provider_configuration(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }
    /// Returns the exact compiled behavior-bearing provider configuration.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }
    /// Returns the exact persisted descriptor-evidence identity.
    #[must_use]
    pub const fn descriptor_evidence(&self) -> Digest {
        self.descriptor_evidence
    }
    /// Returns the authority namespace for the exact account reference.
    #[must_use]
    pub fn account_authority(&self) -> &str {
        &self.account_authority
    }
    /// Returns the non-secret exact account reference.
    #[must_use]
    pub fn account_reference(&self) -> &str {
        &self.account_reference
    }
    /// Returns the account-reference semantic revision.
    #[must_use]
    pub const fn account_revision(&self) -> u64 {
        self.account_revision
    }
    /// Returns the authority namespace for the exact adapter/profile reference.
    #[must_use]
    pub fn profile_authority(&self) -> &str {
        &self.profile_authority
    }
    /// Returns the non-secret exact adapter/profile reference.
    #[must_use]
    pub fn profile_reference(&self) -> &str {
        &self.profile_reference
    }
    /// Returns the adapter/profile reference semantic revision.
    #[must_use]
    pub const fn profile_revision(&self) -> u64 {
        self.profile_revision
    }
    /// Returns when the target admission was freshly observed.
    #[must_use]
    pub const fn admission_observed_at(&self) -> Timestamp {
        self.admission_observed_at
    }
    /// Returns the recorded target-admission expiry, when bounded.
    #[must_use]
    pub const fn admission_valid_until(&self) -> Option<Timestamp> {
        self.admission_valid_until
    }
    /// Returns the exact sealed admission-evidence identity.
    #[must_use]
    pub const fn admission_evidence(&self) -> Digest {
        self.admission_evidence
    }
}

impl fmt::Debug for SessionBindingReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionBindingReport")
            .field("protocol", &self.protocol)
            .field("route", &self.route)
            .field("model", &self.model)
            .field("reported_model", &self.reported_model)
            .field("model_family", &self.model_family)
            .field("adapter", &self.adapter)
            .field("binding", &self.binding)
            .field(
                "selected_provider_configuration",
                &self.selected_provider_configuration,
            )
            .field(
                "effective_provider_configuration",
                &self.effective_provider_configuration,
            )
            .field("descriptor_evidence", &self.descriptor_evidence)
            .field("account_authority", &self.account_authority)
            .field("account_reference", &"<redacted>")
            .field("account_revision", &self.account_revision)
            .field("profile_authority", &self.profile_authority)
            .field("profile_reference", &"<redacted>")
            .field("profile_revision", &self.profile_revision)
            .field("admission_observed_at", &self.admission_observed_at)
            .field("admission_valid_until", &self.admission_valid_until)
            .field("admission_evidence", &self.admission_evidence)
            .finish()
    }
}

impl ForkSessionOutcome {
    /// Returns the sequence of the feed record that made this outcome
    /// durable, so a live consumer knows how far to read before it reports
    /// it. `None` for an outcome that published nothing.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        match self {
            Self::Forked { feed_seq, .. } => *feed_seq,
            _ => None,
        }
    }
}

impl RebindSessionOutcome {
    /// Returns the sequence of the feed record that made this outcome
    /// durable, so a live consumer knows how far to read before it reports
    /// it. `None` for an outcome that published nothing.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        match self {
            Self::Rebound { feed_seq, .. } => *feed_seq,
            _ => None,
        }
    }
}
