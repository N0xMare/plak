//! Validated provenance, components and runtime binding implementations.

use super::{
    Arc, BindingReference, Digest, DigestDomain, MAX_PROVENANCE_BYTES, MAX_RUNTIME_COMPONENT_BYTES,
    MAX_RUNTIME_OPERATION_COMPONENT_BYTES, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelRouteBinding, ProducerProvenance, ProtocolId, ReconstructiveComponent, RevalidationFact,
    RouteId, RuntimeComponentKind, RuntimeManifestError, Timestamp, ToolRuntimeManifest,
    binding_group_integrity, bounded_text, component_integrity, expect_component, fmt, portable_id,
    validate_digest,
};

impl ProducerProvenance {
    /// Creates bounded diagnostic provenance. These fields never gate reopen.
    pub fn new(
        producer: impl Into<String>,
        build: impl Into<String>,
        features: impl Into<String>,
    ) -> Result<Self, RuntimeManifestError> {
        let producer = bounded_text(producer.into(), MAX_PROVENANCE_BYTES, "producer")?;
        let build = bounded_text(build.into(), MAX_PROVENANCE_BYTES, "build")?;
        let features = bounded_text(features.into(), MAX_PROVENANCE_BYTES, "features")?;
        Ok(Self {
            producer,
            build,
            features,
        })
    }

    /// Returns the producing component identity.
    #[must_use]
    pub fn producer(&self) -> &str {
        &self.producer
    }

    /// Returns the diagnostic build identity.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// Returns the diagnostic feature-closure identity.
    #[must_use]
    pub fn features(&self) -> &str {
        &self.features
    }
}

impl fmt::Debug for ProducerProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProducerProvenance")
            .field("producer", &self.producer)
            .field("build", &self.build)
            .field("features", &self.features)
            .finish()
    }
}

impl RuntimeComponentKind {
    pub(super) const fn max_bytes(self) -> usize {
        match self {
            Self::Operation => MAX_RUNTIME_OPERATION_COMPONENT_BYTES,
            _ => MAX_RUNTIME_COMPONENT_BYTES,
        }
    }

    pub(super) fn from_tag(tag: u8) -> Result<Self, RuntimeManifestError> {
        match tag {
            1 => Ok(Self::HarnessPlan),
            2 => Ok(Self::ContextState),
            3 => Ok(Self::ModelDescriptor),
            4 => Ok(Self::AdmissionEvidence),
            5 => Ok(Self::ProfileBinding),
            6 => Ok(Self::ToolRegistry),
            7 => Ok(Self::ToolExposure),
            8 => Ok(Self::ExecutionEnvironment),
            9 => Ok(Self::EffectPolicy),
            10 => Ok(Self::Command),
            11 => Ok(Self::Operation),
            12 => Ok(Self::FamilyBinding),
            _ => Err(RuntimeManifestError::UnknownTag {
                field: "runtime component",
                tag,
            }),
        }
    }
}

impl ReconstructiveComponent {
    /// Captures mandatory reconstruction bytes and their integrity identity.
    pub fn new(
        kind: RuntimeComponentKind,
        codec_revision: u32,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, RuntimeManifestError> {
        if codec_revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > kind.max_bytes() {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let integrity = component_integrity(kind, codec_revision, &bytes)?;
        Ok(Self {
            kind,
            codec_revision,
            bytes,
            integrity,
        })
    }

    pub(super) fn from_recorded(
        kind: RuntimeComponentKind,
        codec_revision: u32,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let component = Self::new(kind, codec_revision, bytes)?;
        if component.integrity != integrity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(component)
    }

    /// Returns the closed component meaning.
    #[must_use]
    pub const fn kind(&self) -> RuntimeComponentKind {
        self.kind
    }

    /// Returns the exact component codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    /// Returns the mandatory exact reconstruction bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns integrity evidence without substituting for [`Self::bytes`].
    #[must_use]
    pub const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for ReconstructiveComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconstructiveComponent")
            .field("kind", &self.kind)
            .field("codec_revision", &self.codec_revision)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("integrity", &self.integrity)
            .finish()
    }
}

impl BindingReference {
    /// Creates a bounded reference. It carries no credential or capability.
    pub fn new(
        authority: impl Into<String>,
        subject: impl Into<String>,
        revision: u64,
    ) -> Result<Self, RuntimeManifestError> {
        if revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        Ok(Self {
            authority: portable_id(authority.into(), "binding authority")?,
            subject: portable_id(subject.into(), "binding subject")?,
            revision,
        })
    }

    /// Returns the owning revalidation authority.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Returns the non-secret subject reference.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the exact reference semantics revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl fmt::Debug for BindingReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BindingReference")
            .field("authority", &self.authority)
            .field("subject", &"<redacted>")
            .field("revision", &self.revision)
            .finish()
    }
}

impl RevalidationFact {
    /// Creates a bounded historical fact. Reopen still requires a fresh witness.
    pub fn new(
        reference: BindingReference,
        observed_at: Timestamp,
        valid_until: Option<Timestamp>,
        evidence: ReconstructiveComponent,
    ) -> Result<Self, RuntimeManifestError> {
        expect_component(&evidence, RuntimeComponentKind::AdmissionEvidence)?;
        if valid_until.is_some_and(|until| until <= observed_at) {
            return Err(RuntimeManifestError::InvalidRevalidation);
        }
        Ok(Self {
            reference,
            observed_at,
            valid_until,
            evidence,
        })
    }

    /// Returns the stable external reference.
    #[must_use]
    pub const fn reference(&self) -> &BindingReference {
        &self.reference
    }

    /// Returns when this evidence was observed.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the historical expiry, if any.
    #[must_use]
    pub const fn valid_until(&self) -> Option<Timestamp> {
        self.valid_until
    }

    /// Returns exact evidence bytes.
    #[must_use]
    pub const fn evidence(&self) -> &ReconstructiveComponent {
        &self.evidence
    }
}

impl ModelRouteBinding {
    /// Creates the exact binding selected before dispatch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed provider binding"
    )]
    pub fn new(
        protocol: ProtocolId,
        route: RouteId,
        requested_model: ModelId,
        reported_model: Option<ModelId>,
        model_family: Option<ModelFamilyId>,
        adapter: ModelAdapterProfileId,
        account: BindingReference,
        profile: BindingReference,
        model_binding: Digest,
        descriptor: ReconstructiveComponent,
        admission: RevalidationFact,
    ) -> Result<Self, RuntimeManifestError> {
        validate_digest(model_binding, DigestDomain::ModelBinding)?;
        expect_component(&descriptor, RuntimeComponentKind::ModelDescriptor)?;
        Ok(Self {
            protocol,
            route,
            requested_model,
            reported_model,
            model_family,
            adapter,
            account,
            profile,
            model_binding,
            descriptor,
            admission,
        })
    }

    /// Returns the exact descriptor binding identity.
    #[must_use]
    pub const fn model_binding(&self) -> Digest {
        self.model_binding
    }

    /// Returns the exact selected route/account/profile/admission binding.
    pub fn route_binding_identity(&self) -> Result<Digest, RuntimeManifestError> {
        let group = binding_group_integrity(self)?;
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-agent/model-route-binding-v1",
                &group.identity_bytes(),
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)
    }

    /// Returns the account authority reference.
    #[must_use]
    pub const fn account(&self) -> &BindingReference {
        &self.account
    }

    /// Returns the profile authority reference.
    #[must_use]
    pub const fn profile(&self) -> &BindingReference {
        &self.profile
    }

    /// Returns the exact descriptor reconstruction component.
    #[must_use]
    pub const fn descriptor(&self) -> &ReconstructiveComponent {
        &self.descriptor
    }

    /// Returns the historical admission/revalidation evidence.
    #[must_use]
    pub const fn admission(&self) -> &RevalidationFact {
        &self.admission
    }

    /// Returns the exact protocol dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the exact route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the exact requested model.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }

    /// Returns the provider-reported model identity, when recorded.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }

    /// Returns the resolved model family, when recorded.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the exact selected model-adapter profile.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }
}

impl ToolRuntimeManifest {
    /// Creates a complete non-secret tool/host binding.
    pub fn new(
        registry: ReconstructiveComponent,
        exposure: ReconstructiveComponent,
        environment: ReconstructiveComponent,
        effect_policy: ReconstructiveComponent,
        policy: BindingReference,
    ) -> Result<Self, RuntimeManifestError> {
        expect_component(&registry, RuntimeComponentKind::ToolRegistry)?;
        expect_component(&exposure, RuntimeComponentKind::ToolExposure)?;
        expect_component(&environment, RuntimeComponentKind::ExecutionEnvironment)?;
        expect_component(&effect_policy, RuntimeComponentKind::EffectPolicy)?;
        Ok(Self {
            registry,
            exposure,
            environment,
            effect_policy,
            policy,
        })
    }

    /// Returns the exact frozen registry component.
    #[must_use]
    pub const fn registry(&self) -> &ReconstructiveComponent {
        &self.registry
    }

    /// Returns the exact frozen exposure component.
    #[must_use]
    pub const fn exposure(&self) -> &ReconstructiveComponent {
        &self.exposure
    }

    /// Returns the exact environment facts.
    #[must_use]
    pub const fn environment(&self) -> &ReconstructiveComponent {
        &self.environment
    }

    /// Returns the exact effect policy facts.
    #[must_use]
    pub const fn effect_policy(&self) -> &ReconstructiveComponent {
        &self.effect_policy
    }

    /// Returns the live policy revalidation reference.
    #[must_use]
    pub const fn policy(&self) -> &BindingReference {
        &self.policy
    }
}
