use super::{
    AttemptId, CallId, ClaimedToolInvocation, Digest, DigestDomain, EFFECT_DISPATCH_INTENT_VERSION,
    EffectAuthorizationGrant, EffectAuthorizationGrantSource, EffectDispatchIntent,
    RegisteredToolIdentity, RunId, ToolDispatchError, ToolEffect, ToolExposureBinding,
    ToolInvocation, ToolPolicyRevision, ToolRuntimeErrorKind, TurnId, effect_safety_error, fmt,
};

impl EffectDispatchIntent {
    pub(super) fn for_invocation(
        invocation: &ToolInvocation,
        grant: &EffectAuthorizationGrant,
    ) -> Result<Self, ToolDispatchError> {
        let binding = grant.binding();
        let identity = invocation.identity().clone();
        let exposure = invocation.exposure().cloned();
        let revision_bytes = binding.policy_revision().revision().to_be_bytes();
        let exposure_name = exposure
            .as_ref()
            .map_or(b"<canonical>".as_slice(), |value| {
                value.exposed_name().as_str().as_bytes()
            });
        let definition_digest = identity.definition_digest().identity_bytes();
        let schema_digest = identity.schema_digest().identity_bytes();
        let argument_limits_digest = identity.argument_limits_digest().identity_bytes();
        let effect_metadata_digest = identity.effect_metadata_digest().identity_bytes();
        let exposure_digest = exposure.as_ref().map_or_else(
            || b"none".to_vec(),
            |value| value.map_digest().identity_bytes(),
        );
        let argument_digest = binding.argument_digest().identity_bytes();
        let environment_digest = binding.environment_descriptor().digest().identity_bytes();
        let authorization_digest = binding.digest().identity_bytes();
        let (grant_kind, decision_audit_id) = match grant.source() {
            EffectAuthorizationGrantSource::Policy => (b"policy".as_slice(), b"".as_slice()),
            EffectAuthorizationGrantSource::Operator(decision) => {
                (b"operator".as_slice(), decision.as_str().as_bytes())
            }
            _ => return Err(effect_safety_error("unsupported tool policy grant source")),
        };
        let run = invocation.run_id().as_uuid();
        let turn = invocation.turn_id().as_uuid();
        let attempt = invocation.attempt_id().as_uuid();
        let call = invocation.call_id().as_uuid();
        let digest = Digest::sha256(
            DigestDomain::EffectAuthorization,
            EFFECT_DISPATCH_INTENT_VERSION,
            &[
                b"plakous-tools/effect-dispatch-intent",
                run.as_bytes(),
                turn.as_bytes(),
                attempt.as_bytes(),
                call.as_bytes(),
                identity.tool_id().as_str().as_bytes(),
                identity.name().as_str().as_bytes(),
                identity.semantic_version().as_bytes(),
                &definition_digest,
                &schema_digest,
                &argument_limits_digest,
                identity.executor_id().as_str().as_bytes(),
                &effect_metadata_digest,
                exposure_name,
                &exposure_digest,
                &argument_digest,
                &environment_digest,
                binding.policy_revision().policy_id().as_bytes(),
                &revision_bytes,
                grant_kind,
                decision_audit_id,
                &authorization_digest,
                b"mutating",
                b"fixed-manual-no-key-v1",
            ],
        )
        .map_err(|_error| {
            ToolDispatchError::new(
                ToolRuntimeErrorKind::EffectSafety,
                "effect dispatch intent could not be digested",
            )
        })?;
        Ok(Self {
            digest,
            run_id: invocation.run_id(),
            turn_id: invocation.turn_id(),
            attempt_id: invocation.attempt_id(),
            call_id: invocation.call_id(),
            identity,
            exposure,
            argument_digest: binding.argument_digest(),
            environment_digest: binding.environment_descriptor().digest(),
            policy_revision: binding.policy_revision().clone(),
            grant_source: grant.source().clone(),
            authorization_digest: binding.digest(),
            effect: ToolEffect::Mutating,
        })
    }

    /// Returns the complete intent digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Returns the canonical run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the canonical turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns model-attempt lineage.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the canonical tool-call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the frozen canonical tool/executor identity.
    #[must_use]
    pub const fn identity(&self) -> &RegisteredToolIdentity {
        &self.identity
    }

    /// Returns the exact provider-facing exposure binding, when used.
    #[must_use]
    pub const fn exposure(&self) -> Option<&ToolExposureBinding> {
        self.exposure.as_ref()
    }

    /// Returns the normalized argument digest.
    #[must_use]
    pub const fn argument_digest(&self) -> Digest {
        self.argument_digest
    }

    /// Returns the exact execution-environment authority digest.
    #[must_use]
    pub const fn environment_digest(&self) -> Digest {
        self.environment_digest
    }

    /// Returns the frozen policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> &ToolPolicyRevision {
        &self.policy_revision
    }

    /// Returns whether policy or an exact operator decision supplied the grant.
    #[must_use]
    pub const fn grant_source(&self) -> &EffectAuthorizationGrantSource {
        &self.grant_source
    }

    /// Returns the complete policy/decision authorization digest.
    #[must_use]
    pub const fn authorization_digest(&self) -> Digest {
        self.authorization_digest
    }

    /// Returns the canonical side-effect class.
    #[must_use]
    pub const fn effect(&self) -> ToolEffect {
        self.effect
    }
}

impl fmt::Debug for EffectDispatchIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectDispatchIntent")
            .field("digest", &self.digest)
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("identity", &self.identity)
            .field("exposure", &self.exposure)
            .field("argument_digest", &self.argument_digest)
            .field("environment_digest", &self.environment_digest)
            .field("policy_revision", &self.policy_revision)
            .field("grant_source", &self.grant_source)
            .field("authorization_digest", &self.authorization_digest)
            .field("effect", &self.effect)
            .finish_non_exhaustive()
    }
}

impl ClaimedToolInvocation {
    /// Returns the exact intent durably committed before this capability existed.
    #[must_use]
    pub const fn intent(&self) -> &EffectDispatchIntent {
        &self.intent
    }

    /// Returns the exact persisted claim evidence authorizing this dispatch.
    #[must_use]
    pub const fn claim_evidence_digest(&self) -> Digest {
        self.claim_evidence_digest
    }
}

impl fmt::Debug for ClaimedToolInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimedToolInvocation")
            .field("intent", &self.intent)
            .field("claim_evidence_digest", &self.claim_evidence_digest)
            .field("authority", &"<bound>")
            .finish_non_exhaustive()
    }
}
