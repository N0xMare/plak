//! Object-safe executable boundary and correlated invocation values.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{AttemptId, CallId, Digest, RunId, ToolCorrelation, ToolName, TurnId};

use crate::registry::RegistryProvenance;
use crate::{
    RegisteredToolIdentity, ToolContext, ToolOutcome, ToolRuntimeError, ValidatedArguments,
};

/// Owned future returned by an object-safe [`ToolExecutor`].
pub type ToolFuture =
    Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolRuntimeError>> + Send + 'static>>;

/// Minimal provider-neutral executable tool boundary.
pub trait ToolExecutor: Send + Sync + 'static {
    /// Executes one fully validated, owned invocation.
    ///
    /// Implementations own every resource they start. After cancellation they
    /// must finish child/resource cleanup before this future completes.
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture;
}

/// Untrusted exact call material presented to a frozen registry.
#[derive(Clone)]
pub struct ToolInvocationRequest {
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    tool: ToolName,
    correlation: ToolCorrelation,
    raw_arguments: Arc<str>,
}

impl fmt::Debug for ToolInvocationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolInvocationRequest")
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("tool", &self.tool)
            .field("correlation", &"<redacted>")
            .field("raw_argument_bytes", &self.raw_arguments.len())
            .field("raw_arguments", &"<redacted>")
            .finish()
    }
}

impl ToolInvocationRequest {
    /// Creates an exact invocation request. Argument syntax and size are checked
    /// by the frozen registry before execution.
    #[must_use]
    pub fn new(
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        call_id: CallId,
        tool: ToolName,
        correlation: ToolCorrelation,
        raw_arguments: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            run_id,
            turn_id,
            attempt_id,
            call_id,
            tool,
            correlation,
            raw_arguments: raw_arguments.into(),
        }
    }

    /// Returns the canonical run ID.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the canonical turn ID.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the unique in-process attempt ID.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the canonical call ID.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the requested registered tool name.
    #[must_use]
    pub const fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// Returns remote correlation retained for the canonical result.
    #[must_use]
    pub const fn correlation(&self) -> &ToolCorrelation {
        &self.correlation
    }

    /// Returns exact assembled argument text for strict parsing only.
    #[must_use]
    pub(crate) fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }

    pub(crate) fn with_tool(mut self, tool: ToolName) -> Self {
        self.tool = tool;
        self
    }
}

/// Frozen identity and strictly validated arguments delivered to an executor.
#[derive(Clone)]
pub struct ToolInvocation {
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: RegisteredToolIdentity,
    correlation: ToolCorrelation,
    arguments: ValidatedArguments,
    exposure: Option<ToolExposureBinding>,
    effect_dispatch: Option<EffectExecutionBinding>,
    provenance: Arc<RegistryProvenance>,
}

impl fmt::Debug for ToolInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolInvocation")
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("identity", &self.identity)
            .field("correlation", &"<redacted>")
            .field("arguments", &self.arguments)
            .field("exposure", &self.exposure)
            .field("effect_dispatch", &self.effect_dispatch)
            .field("provenance", &"<bound>")
            .finish()
    }
}

impl ToolInvocation {
    pub(crate) fn validated(
        request: ToolInvocationRequest,
        identity: RegisteredToolIdentity,
        arguments: ValidatedArguments,
        provenance: Arc<RegistryProvenance>,
    ) -> Self {
        Self {
            run_id: request.run_id,
            turn_id: request.turn_id,
            attempt_id: request.attempt_id,
            call_id: request.call_id,
            identity,
            correlation: request.correlation,
            arguments,
            exposure: None,
            effect_dispatch: None,
            provenance,
        }
    }

    /// Returns the canonical run ID.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the canonical turn ID.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the unique in-process attempt ID.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the canonical call ID.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the immutable registered identity.
    #[must_use]
    pub const fn identity(&self) -> &RegisteredToolIdentity {
        &self.identity
    }

    /// Returns remote correlation and lineage.
    #[must_use]
    pub const fn correlation(&self) -> &ToolCorrelation {
        &self.correlation
    }

    /// Returns strictly parsed, schema-validated arguments.
    #[must_use]
    pub const fn arguments(&self) -> &ValidatedArguments {
        &self.arguments
    }

    /// Returns provider-facing identity binding when prepared through an
    /// exposure map.
    #[must_use]
    pub const fn exposure(&self) -> Option<&ToolExposureBinding> {
        self.exposure.as_ref()
    }

    /// Returns the exact committed effect dispatch binding for a mutating executor.
    #[must_use]
    pub const fn effect_dispatch(&self) -> Option<&EffectExecutionBinding> {
        self.effect_dispatch.as_ref()
    }

    pub(crate) fn with_exposure(mut self, exposed_name: ToolName, map_digest: Digest) -> Self {
        self.exposure = Some(ToolExposureBinding {
            canonical_identity: self.identity.clone(),
            exposed_name,
            map_digest,
        });
        self
    }

    pub(crate) fn with_effect_dispatch(
        mut self,
        intent_digest: Digest,
        claim_evidence_digest: Digest,
    ) -> Self {
        self.effect_dispatch = Some(EffectExecutionBinding {
            intent_digest,
            claim_evidence_digest,
        });
        self
    }

    pub(crate) const fn provenance(&self) -> &Arc<RegistryProvenance> {
        &self.provenance
    }
}

/// Exact durably claimed effect facts delivered to the mutating executor.
#[derive(Clone, Eq, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "the digest suffix distinguishes typed digest values from the semantic facts they bind"
)]
pub struct EffectExecutionBinding {
    intent_digest: Digest,
    claim_evidence_digest: Digest,
}

impl EffectExecutionBinding {
    /// Returns the exact committed dispatch-intent digest.
    #[must_use]
    pub const fn intent_digest(&self) -> Digest {
        self.intent_digest
    }

    /// Returns the exact persisted claim-evidence digest authorizing dispatch.
    #[must_use]
    pub const fn claim_evidence_digest(&self) -> Digest {
        self.claim_evidence_digest
    }
}

impl fmt::Debug for EffectExecutionBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectExecutionBinding")
            .field("intent_digest", &self.intent_digest)
            .field("claim_evidence_digest", &self.claim_evidence_digest)
            .finish()
    }
}

/// Immutable provider-facing identity retained on an exposed invocation.
///
/// Approval and audit layers bind this value together with the invocation's
/// normalized argument digest, execution environment, and policy revision.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolExposureBinding {
    canonical_identity: RegisteredToolIdentity,
    exposed_name: ToolName,
    map_digest: Digest,
}

impl ToolExposureBinding {
    /// Returns the exact registered identity selected for execution.
    #[must_use]
    pub const fn canonical_identity(&self) -> &RegisteredToolIdentity {
        &self.canonical_identity
    }

    /// Returns the provider-facing name observed in the model request.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the exact frozen exposure-map digest.
    #[must_use]
    pub const fn map_digest(&self) -> Digest {
        self.map_digest
    }
}

impl fmt::Debug for ToolExposureBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExposureBinding")
            .field("canonical_identity", &self.canonical_identity)
            .field("exposed_name", &self.exposed_name)
            .field("map_digest", &self.map_digest)
            .finish()
    }
}

/// Compile-time assertion that the public boundary remains object-safe.
#[doc(hidden)]
pub fn assert_tool_executor_object_safe(_: &dyn ToolExecutor) {}
