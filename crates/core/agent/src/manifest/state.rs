//! Canonical state, topology, family artifacts and interpreter observations.

use super::{
    ActiveFamilyArtifact, Arc, CanonicalHistory, ConversationView, Digest, DigestDomain,
    FamilyArtifact, InterpreterAcknowledgedBoundary, InterpreterAvailability,
    InterpreterObservation, InterpreterResetReason, MAX_ID_BYTES, MAX_RUNTIME_COMPONENT_BYTES,
    OutputBoundRederived, ReconstructiveComponent, RuntimeCanonicalState, RuntimeContextState,
    RuntimeManifestError, RuntimeTopology, SealedFamilyArtifactCandidate, StableRequestPrefix,
    WorkingProjection, bounded_text, family_artifact_integrity, family_binding_component,
    family_binding_digest, fmt, portable_id, supported_family_artifact_revision, validate_digest,
};

impl RuntimeTopology {
    pub(super) fn validate(self) -> Result<Self, RuntimeManifestError> {
        if self.lease_fence == 0
            || self.parent_checkpoint_id == Some(self.checkpoint_id)
            || self.fork.is_some_and(|fork| {
                fork.session_id == self.session_id
                    && fork.branch_id == self.branch_id
                    && fork.checkpoint_id == self.checkpoint_id
            })
        {
            return Err(RuntimeManifestError::InvalidTopology);
        }
        validate_digest(self.commit_identity, DigestDomain::ProviderOperation)?;
        Ok(self)
    }
}

impl RuntimeCanonicalState {
    /// Captures exact canonical/model-visible reconstruction data.
    pub fn new(
        stable_prefix: StableRequestPrefix,
        history: CanonicalHistory,
        working: WorkingProjection,
        context: RuntimeContextState,
    ) -> Result<Self, RuntimeManifestError> {
        context.validate_history(&history)?;
        Ok(Self {
            stable_prefix,
            history,
            working,
            context,
        })
    }

    /// Returns the exact stable request prefix.
    #[must_use]
    pub const fn stable_prefix(&self) -> &StableRequestPrefix {
        &self.stable_prefix
    }

    /// Returns committed canonical history.
    #[must_use]
    pub const fn history(&self) -> &CanonicalHistory {
        &self.history
    }

    /// Returns the bounded working projection.
    #[must_use]
    pub const fn working(&self) -> &WorkingProjection {
        &self.working
    }

    /// Returns exact context/compaction facts.
    #[must_use]
    pub const fn context(&self) -> &RuntimeContextState {
        &self.context
    }

    /// Returns the exact committed-plus-working model-visible projection.
    #[must_use]
    pub fn projection(&self) -> ConversationView {
        ConversationView::from_shared(self.history.clone(), self.working.clone())
    }
}

impl FamilyArtifact {
    /// Captures exact family-native bytes. The family kernel remains their validator.
    pub fn new(
        family: impl Into<String>,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, RuntimeManifestError> {
        if schema_revision == 0 || codec_revision == 0 {
            return Err(RuntimeManifestError::ZeroRevision);
        }
        validate_digest(binding, DigestDomain::ModelBinding)?;
        let family = portable_id(family.into(), "protocol family")?;
        if !supported_family_artifact_revision(&family, schema_revision, codec_revision) {
            return Err(RuntimeManifestError::RevisionDisagreement);
        }
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_RUNTIME_COMPONENT_BYTES {
            return Err(RuntimeManifestError::ComponentSize);
        }
        let binding_component = family_binding_component(binding)?;
        let integrity = family_artifact_integrity(
            &family,
            schema_revision,
            codec_revision,
            &binding_component,
            &bytes,
        )?;
        Ok(Self {
            family,
            schema_revision,
            codec_revision,
            binding,
            binding_component,
            bytes,
            integrity,
        })
    }

    pub(super) fn from_recorded(
        family: String,
        schema_revision: u32,
        codec_revision: u32,
        binding_component: &ReconstructiveComponent,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, RuntimeManifestError> {
        let binding = family_binding_digest(binding_component)?;
        let artifact = Self::new(family, schema_revision, codec_revision, binding, bytes)?;
        if artifact.binding_component != *binding_component {
            return Err(RuntimeManifestError::Malformed);
        }
        if artifact.integrity != integrity {
            return Err(RuntimeManifestError::IntegrityMismatch);
        }
        Ok(artifact)
    }

    /// Converts one fully sealed terminal candidate without decoding family bytes.
    pub fn from_candidate(
        candidate: &SealedFamilyArtifactCandidate,
    ) -> Result<Self, RuntimeManifestError> {
        Self::new(
            candidate.family(),
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            candidate.bytes(),
        )
    }

    /// Converts the durable envelope into preparation-independent model-call state.
    pub fn to_active(&self) -> Result<ActiveFamilyArtifact, RuntimeManifestError> {
        ActiveFamilyArtifact::new(
            self.family.clone(),
            self.schema_revision,
            self.codec_revision,
            self.binding,
            Arc::clone(&self.bytes),
        )
        .map_err(|_| RuntimeManifestError::Malformed)
    }

    /// Returns the family identity (for example `responses`).
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Returns the exact artifact bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the exact producing artifact binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }

    pub(crate) const fn binding_component(&self) -> &ReconstructiveComponent {
        &self.binding_component
    }

    /// Returns the behavior-bearing family schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    /// Returns the exact family codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    pub(crate) const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for FamilyArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FamilyArtifact")
            .field("family", &self.family)
            .field("schema_revision", &self.schema_revision)
            .field("codec_revision", &self.codec_revision)
            .field("binding", &self.binding)
            .field("binding_component", &"<redacted binding component>")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("integrity", &self.integrity)
            .finish()
    }
}

impl InterpreterObservation {
    /// Creates one validated closed observation.
    pub fn new(
        generation: u64,
        availability: InterpreterAvailability,
        reset_reason: Option<InterpreterResetReason>,
        affected_cell: Option<String>,
        last_acknowledged: Option<InterpreterAcknowledgedBoundary>,
    ) -> Result<Self, RuntimeManifestError> {
        if generation == 0
            || matches!(availability, InterpreterAvailability::Available) != reset_reason.is_none()
        {
            return Err(RuntimeManifestError::InvalidInterpreterObservation);
        }
        let affected_cell = affected_cell
            .map(|cell| bounded_text(cell, MAX_ID_BYTES, "interpreter affected cell"))
            .transpose()?;
        let cell_boundary = matches!(
            last_acknowledged,
            Some(
                InterpreterAcknowledgedBoundary::SourceAccepted
                    | InterpreterAcknowledgedBoundary::ChildResultAccepted
            )
        );
        let cell_consistent = cell_boundary == affected_cell.is_some()
            || matches!(availability, InterpreterAvailability::Reset)
                && last_acknowledged.is_none();
        let availability_consistent = match availability {
            InterpreterAvailability::Available => last_acknowledged
                .is_some_and(|boundary| !matches!(boundary, InterpreterAcknowledgedBoundary::Join)),
            InterpreterAvailability::Reset => true,
        };
        if !cell_consistent || !availability_consistent {
            return Err(RuntimeManifestError::InvalidInterpreterObservation);
        }
        Ok(Self {
            generation,
            availability,
            reset_reason,
            affected_cell,
            last_acknowledged,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub const fn availability(&self) -> InterpreterAvailability {
        self.availability
    }
    #[must_use]
    pub const fn reset_reason(&self) -> Option<InterpreterResetReason> {
        self.reset_reason
    }
    #[must_use]
    pub fn affected_cell(&self) -> Option<&str> {
        self.affected_cell.as_deref()
    }
    #[must_use]
    pub const fn last_acknowledged(&self) -> Option<InterpreterAcknowledgedBoundary> {
        self.last_acknowledged
    }
}

impl OutputBoundRederived {
    /// The bound the session was created under.
    #[must_use]
    pub const fn previous(self) -> Option<std::num::NonZeroU32> {
        self.previous
    }

    /// The bound this reopen derived and will use.
    #[must_use]
    pub const fn current(self) -> Option<std::num::NonZeroU32> {
        self.current
    }
}
