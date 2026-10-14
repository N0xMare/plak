//! Fail-closed tool-policy admission and digest-bound decision values.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, SafeDiagnostic, ToolDefinition};

use crate::{
    ExecutionEnvironment, ExecutionEnvironmentDescriptor, RegisteredToolIdentity,
    ToolExposureBinding, ToolInvocation, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// Approval-binding digest format implemented by this crate.
pub const EFFECT_AUTHORIZATION_BINDING_VERSION: u32 = 6;

/// Owned future returned by an object-safe [`ToolPolicy`].
pub type ToolPolicyFuture =
    Pin<Box<dyn Future<Output = Result<ToolPolicyDecision, ToolRuntimeError>> + Send + 'static>>;

/// Stable identity of one immutable policy configuration.
///
/// A policy implementation must mint a new revision whenever any input that
/// can change its decision changes. Runtime construction freezes this value and
/// fails closed if the implementation later reports a different revision.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolPolicyRevision {
    policy_id: Arc<str>,
    revision: u64,
}

impl ToolPolicyRevision {
    /// Creates a bounded portable policy identity and nonzero revision.
    ///
    /// # Errors
    ///
    /// Returns a policy error for an empty, oversized, non-portable ID or a
    /// zero revision.
    pub fn new(policy_id: impl Into<Arc<str>>, revision: u64) -> Result<Self, ToolRuntimeError> {
        let policy_id = policy_id.into();
        let portable = !policy_id.is_empty()
            && policy_id.len() <= 128
            && policy_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            });
        if !portable || revision == 0 {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Policy,
                "tool policy revision is invalid",
            ));
        }
        Ok(Self {
            policy_id,
            revision,
        })
    }

    /// Returns the stable policy implementation/configuration ID.
    #[must_use]
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    /// Returns the monotonic application-owned revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl fmt::Debug for ToolPolicyRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPolicyRevision")
            .field("policy_id", &self.policy_id)
            .field("revision", &self.revision)
            .finish()
    }
}

/// Exact immutable input evaluated by a [`ToolPolicy`].
#[derive(Clone)]
pub struct ToolPolicyRequest {
    invocation: ToolInvocation,
    definition: ToolDefinition,
    environment: Arc<ExecutionEnvironment>,
    policy_revision: ToolPolicyRevision,
    authorization_binding: EffectAuthorizationBinding,
}

impl ToolPolicyRequest {
    pub(crate) fn new(
        invocation: ToolInvocation,
        definition: ToolDefinition,
        environment: Arc<ExecutionEnvironment>,
        policy_revision: ToolPolicyRevision,
        authorization_binding: EffectAuthorizationBinding,
    ) -> Self {
        Self {
            invocation,
            definition,
            environment,
            policy_revision,
            authorization_binding,
        }
    }

    /// Returns the strictly validated invocation and normalized arguments.
    #[must_use]
    pub const fn invocation(&self) -> &ToolInvocation {
        &self.invocation
    }

    /// Returns the frozen registered definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Returns the exact live execution environment proposed for dispatch.
    #[must_use]
    pub const fn environment(&self) -> &Arc<ExecutionEnvironment> {
        &self.environment
    }

    /// Returns the policy revision frozen by the runtime.
    #[must_use]
    pub const fn policy_revision(&self) -> &ToolPolicyRevision {
        &self.policy_revision
    }

    /// Returns the authoritative digest-bound authorization identity.
    #[must_use]
    pub const fn authorization_binding(&self) -> &EffectAuthorizationBinding {
        &self.authorization_binding
    }
}

impl fmt::Debug for ToolPolicyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPolicyRequest")
            .field("invocation", &self.invocation)
            .field("definition_digest", &self.definition.digest())
            .field("environment", &"<bound>")
            .field("policy_revision", &self.policy_revision)
            .field("authorization_binding", &self.authorization_binding)
            .finish()
    }
}

/// Object-safe, provider-neutral policy boundary evaluated before dispatch.
pub trait ToolPolicy: Any + Send + Sync + 'static {
    /// Returns the exact immutable policy configuration revision.
    fn revision(&self) -> ToolPolicyRevision;

    /// Evaluates one exact validated invocation without performing its effects.
    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture;
}

/// A policy decision. Only `Allowed` can become an automatic grant.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolPolicyDecision {
    /// Permit this exact binding without operator interaction.
    Allowed,
    /// Reject the invocation without executor dispatch.
    Denied(ToolPolicyDenial),
    /// Pause admission until the control plane supplies a decision.
    DecisionRequired(EffectDecisionPrompt),
}

/// Bounded, model-safe policy denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolPolicyDenial {
    reason: SafeDiagnostic,
}

impl ToolPolicyDenial {
    /// Creates a sanitized denial reason.
    #[must_use]
    pub fn new(reason: impl AsRef<str>) -> Self {
        Self {
            reason: SafeDiagnostic::new(reason),
        }
    }

    /// Returns the safe descriptive reason.
    #[must_use]
    pub const fn reason(&self) -> &SafeDiagnostic {
        &self.reason
    }
}

/// Descriptive operator-facing decision prompt.
///
/// This text is never authoritative; [`EffectAuthorizationBinding`] is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectDecisionPrompt {
    description: SafeDiagnostic,
}

impl EffectDecisionPrompt {
    /// Creates sanitized descriptive decision text.
    #[must_use]
    pub fn new(description: impl AsRef<str>) -> Self {
        Self {
            description: SafeDiagnostic::new(description),
        }
    }

    /// Returns the descriptive prompt.
    #[must_use]
    pub const fn description(&self) -> &SafeDiagnostic {
        &self.description
    }
}

/// Stable application-owned identity for an operator decision decision.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EffectDecisionAuditId(Arc<str>);

impl EffectDecisionAuditId {
    /// Creates a bounded portable decision ID.
    ///
    /// # Errors
    ///
    /// Returns a policy error for invalid text.
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ToolRuntimeError> {
        let value = value.into();
        let portable = !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            });
        if !portable {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Policy,
                "tool decision ID is invalid",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the portable decision ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EffectDecisionAuditId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("EffectDecisionAuditId")
            .field(&self.0)
            .finish()
    }
}

/// The disposition selected by a control-plane effect-decision response.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EffectDecisionDisposition {
    /// Allow once under an application-owned audit ID.
    Allow(EffectDecisionAuditId),
    /// Deny with a bounded descriptive reason.
    Deny(ToolPolicyDenial),
}

/// Binding-addressed control-plane response for one pending request.
///
/// Carrying the digest observed by the operator/UI prevents a response for one
/// pending request from being accidentally applied to another.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectDecision {
    binding_digest: Digest,
    disposition: EffectDecisionDisposition,
}

impl EffectDecision {
    /// Creates an decision for the exact displayed binding digest.
    #[must_use]
    pub const fn allow(binding_digest: Digest, decision_audit_id: EffectDecisionAuditId) -> Self {
        Self {
            binding_digest,
            disposition: EffectDecisionDisposition::Allow(decision_audit_id),
        }
    }

    /// Creates a denial for the exact displayed binding digest.
    #[must_use]
    pub const fn deny(binding_digest: Digest, denial: ToolPolicyDenial) -> Self {
        Self {
            binding_digest,
            disposition: EffectDecisionDisposition::Deny(denial),
        }
    }

    /// Returns the authoritative binding digest observed by the control plane.
    #[must_use]
    pub const fn binding_digest(&self) -> Digest {
        self.binding_digest
    }

    /// Returns the requested disposition.
    #[must_use]
    pub const fn disposition(&self) -> &EffectDecisionDisposition {
        &self.disposition
    }

    pub(crate) fn into_disposition(self) -> EffectDecisionDisposition {
        self.disposition
    }
}

/// Exact fields and digest to which policy or operator decision applies.
#[derive(Clone, Eq, PartialEq)]
pub struct EffectAuthorizationBinding {
    digest: Digest,
    identity: RegisteredToolIdentity,
    argument_digest: Digest,
    environment_descriptor: ExecutionEnvironmentDescriptor,
    policy_revision: ToolPolicyRevision,
    exposure: Option<ToolExposureBinding>,
}

impl EffectAuthorizationBinding {
    pub(crate) fn for_invocation(
        invocation: &ToolInvocation,
        environment_descriptor: ExecutionEnvironmentDescriptor,
        policy_revision: ToolPolicyRevision,
    ) -> Result<Self, ToolRuntimeError> {
        let identity = invocation.identity().clone();
        let argument_digest = invocation.arguments().digest();
        let exposure = invocation.exposure().cloned();
        let run = invocation.run_id().as_uuid();
        let turn = invocation.turn_id().as_uuid();
        let attempt = invocation.attempt_id().as_uuid();
        let call = invocation.call_id().as_uuid();
        let revision_bytes = policy_revision.revision.to_be_bytes();
        let exposure_name = exposure
            .as_ref()
            .map_or(b"<canonical>".as_slice(), |binding| {
                binding.exposed_name().as_str().as_bytes()
            });
        let definition_digest = identity.definition_digest().identity_bytes();
        let schema_digest = identity.schema_digest().identity_bytes();
        let argument_limits_digest = identity.argument_limits_digest().identity_bytes();
        let effect_metadata_digest = identity.effect_metadata_digest().identity_bytes();
        let executor_binding_identity = identity
            .executor_binding_identity()
            .digest()
            .identity_bytes();
        let argument_digest_identity = argument_digest.identity_bytes();
        let environment_digest = environment_descriptor.digest().identity_bytes();
        let exposure_digest = exposure.as_ref().map_or_else(
            || b"none".to_vec(),
            |binding| binding.map_digest().identity_bytes(),
        );
        let digest = Digest::sha256(
            DigestDomain::EffectAuthorization,
            EFFECT_AUTHORIZATION_BINDING_VERSION,
            &[
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
                &executor_binding_identity,
                &effect_metadata_digest,
                &argument_digest_identity,
                &environment_digest,
                policy_revision.policy_id.as_bytes(),
                &revision_bytes,
                exposure_name,
                &exposure_digest,
            ],
        )
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Policy,
                "tool authorization binding could not be computed",
                &error,
            )
        })?;
        Ok(Self {
            digest,
            identity,
            argument_digest,
            environment_descriptor,
            policy_revision,
            exposure,
        })
    }

    /// Returns the authoritative authorization digest.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Returns the exact frozen registered tool identity.
    #[must_use]
    pub const fn identity(&self) -> &RegisteredToolIdentity {
        &self.identity
    }

    /// Returns the normalized argument digest.
    #[must_use]
    pub const fn argument_digest(&self) -> Digest {
        self.argument_digest
    }

    /// Returns the exact environment identity.
    #[must_use]
    pub const fn environment_descriptor(&self) -> &ExecutionEnvironmentDescriptor {
        &self.environment_descriptor
    }

    /// Returns the exact policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> &ToolPolicyRevision {
        &self.policy_revision
    }

    /// Returns the provider-facing exposure binding, when applicable.
    #[must_use]
    pub const fn exposure(&self) -> Option<&ToolExposureBinding> {
        self.exposure.as_ref()
    }
}

impl fmt::Debug for EffectAuthorizationBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectAuthorizationBinding")
            .field("digest", &self.digest)
            .field("identity", &self.identity)
            .field("argument_digest", &self.argument_digest)
            .field("environment_descriptor", &self.environment_descriptor)
            .field("policy_revision", &self.policy_revision)
            .field("exposure", &self.exposure)
            .finish()
    }
}

/// Origin of one opaque dispatch grant.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EffectAuthorizationGrantSource {
    /// The frozen policy allowed this exact binding automatically.
    Policy,
    /// The control plane allowed this exact pending binding.
    Operator(EffectDecisionAuditId),
}

/// Opaque grant carried in [`crate::ToolContext`] and audit paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectAuthorizationGrant {
    binding: EffectAuthorizationBinding,
    source: EffectAuthorizationGrantSource,
}

impl EffectAuthorizationGrant {
    pub(crate) const fn new(
        binding: EffectAuthorizationBinding,
        source: EffectAuthorizationGrantSource,
    ) -> Self {
        Self { binding, source }
    }

    /// Returns the authoritative exact binding.
    #[must_use]
    pub const fn binding(&self) -> &EffectAuthorizationBinding {
        &self.binding
    }

    /// Returns whether policy or an operator admitted the invocation.
    #[must_use]
    pub const fn source(&self) -> &EffectAuthorizationGrantSource {
        &self.source
    }
}

/// Safe default for the early in-memory agent: permit read-only definitions and
/// deny every mutating definition.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadOnlyToolPolicy;

impl ToolPolicy for ReadOnlyToolPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision {
            policy_id: Arc::from("plakous.read-only"),
            revision: 1,
        }
    }

    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        let decision = match request.definition().effect() {
            plakous_model::ToolEffect::ReadOnly => ToolPolicyDecision::Allowed,
            plakous_model::ToolEffect::Mutating => ToolPolicyDecision::Denied(
                ToolPolicyDenial::new("mutating tools are denied by the read-only policy"),
            ),
            _ => ToolPolicyDecision::Denied(ToolPolicyDenial::new(
                "unknown tool effects are denied by the read-only policy",
            )),
        };
        Box::pin(async move { Ok(decision) })
    }
}

/// Permits every registered tool definition, including mutating ones.
///
/// Use only when the host has already selected a full tool pack and a
/// required-durability session store (for example `plak run --tools full`).
/// Unknown effect classes still fail closed.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlwaysAllowPolicy;

impl ToolPolicy for AlwaysAllowPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision {
            policy_id: Arc::from("plakous.allow-registered"),
            revision: 1,
        }
    }

    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        let decision = match request.definition().effect() {
            plakous_model::ToolEffect::ReadOnly | plakous_model::ToolEffect::Mutating => {
                ToolPolicyDecision::Allowed
            }
            _ => ToolPolicyDecision::Denied(ToolPolicyDenial::new(
                "unknown tool effects are denied by the allow-registered policy",
            )),
        };
        Box::pin(async move { Ok(decision) })
    }
}

/// Compile-time assertion that the public policy boundary remains object-safe.
#[doc(hidden)]
pub fn assert_tool_policy_object_safe(_: &dyn ToolPolicy) {}
