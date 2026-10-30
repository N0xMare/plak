//! Object-safe model calls, cancellation context, facets, and typed errors.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;

use crate::{AttemptId, ModelEvent, ModelOperationRoot, RouteId, Timestamp, ValidatedRequestPlan};

mod error;

pub use error::{
    ContextCapacityFailure, MAX_MODEL_PAID_RECOVERY_BYTES, MAX_MODEL_PROVIDER_CODE_BYTES,
    MAX_MODEL_PROVIDER_REQUEST_ID_BYTES, MAX_SERVED_BY_BYTES, ModelDispatchRejection,
    ModelDispatchRejectionClass, ModelDispatchRejectionPhase, ModelError, ModelErrorContext,
    ModelErrorContextError, ModelErrorKind, ModelErrorPhase, ModelErrorTarget, ModelHttpStatus,
    ModelPaidProviderAcceptance, ModelPaidRecovery, ModelPaidRecoveryBytes,
    ModelPaidRecoveryRequirement, ModelPaidTerminalFacts, ModelProviderCode,
    ModelProviderRequestId, ModelRetryDelay, ModelRetryRisk, ModelRetryability,
    RedactedModelErrorSource, ServedBy, provider_rejection_is_context_overflow,
};

mod authority;
mod cancellation;
mod family_artifact;
mod preparation;
mod prepared_call;
mod recovery;

pub use authority::{
    MAX_RUNTIME_ADMISSION_WITNESS_BYTES, RuntimeAdmissionWitness, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, RuntimeModelAuthorityWaitOperation,
};
pub use cancellation::{CancellationFuture, CancellationToken};
pub use family_artifact::{
    ActiveFamilyArtifact, FamilyArtifactError, FamilyArtifactProducerFacts,
    MAX_FAMILY_ARTIFACT_BYTES,
};
pub(crate) use family_artifact::{
    family_artifact_integrity, validate_family_bytes, validate_family_digest, validate_family_id,
    validate_family_revisions,
};
pub use preparation::{
    MAX_PREPARED_WIRE_BYTES, ModelCacheAffinity, ModelCacheAffinityError, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, PreparedCacheUse, PreparedModelProjection,
    PreparedOutputLimit, PreparedOutputLimitSource, PreparedTokenProjection, TurnPosition,
};
pub use prepared_call::{PreparedCallError, PreparedModelCall};
pub use recovery::{
    MAX_PREPARED_CALL_RECORD_BYTES, MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES,
    MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION, MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION,
    MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION, ModelCallRecoveryError,
    ModelCallRecoveryEvidence, ModelCallRecoveryOutcome, ModelCallRecoveryRequest,
    ModelCallRecoveryResult, PREPARED_CALL_RECORD_SEMANTIC_REVISION,
    PROVIDER_OPERATION_SEMANTIC_REVISION, PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION,
    PreparedCallRecord, ProviderCallRecoveryLocator, ProviderCallRecoveryLocatorStage,
};

/// Owned context shared by preparation-independent model work.
#[derive(Clone, Debug)]
pub struct ModelCallContext {
    attempt_id: AttemptId,
    cancellation: CancellationToken,
    deadline: Option<Timestamp>,
    dispatch_timeout: Option<std::time::Duration>,
}

impl ModelCallContext {
    /// Creates a model call context.
    #[must_use]
    pub fn new(
        attempt_id: AttemptId,
        cancellation: CancellationToken,
        deadline: Option<Timestamp>,
    ) -> Self {
        Self {
            attempt_id,
            cancellation,
            deadline,
            dispatch_timeout: None,
        }
    }

    /// Returns the attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Sets the budget of each physical dispatch, up to its response head.
    /// This is not a deadline for the logical call or a streaming body.
    #[must_use]
    pub const fn with_dispatch_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.dispatch_timeout = Some(timeout);
        self
    }

    /// Returns the caller's per-generation dispatch budget, when supplied.
    #[must_use]
    pub const fn dispatch_timeout(&self) -> Option<std::time::Duration> {
        self.dispatch_timeout
    }

    /// Returns an optional hard deadline for the entire logical call.
    #[must_use]
    pub const fn deadline(&self) -> Option<Timestamp> {
        self.deadline
    }
}

/// An owned structural model-event stream.
///
/// Nonterminal events are bounded best-effort observations. They may be
/// dropped or become structurally gapped without changing the operation's
/// semantic outcome. A successful stream's unique
/// [`ModelEvent::ResponseCompleted`] is the self-contained authoritative
/// aggregate; consumers validate it against the prepared plan rather than
/// reconstructing canonical state from deltas.
///
/// Dropping the sole stream destroys its only in-band result channel, so a
/// provider cannot emit a later [`ModelError`] to that caller from `Drop` or
/// unwind. Drop is never terminal paid-dispatch proof. Durable paid recovery
/// therefore belongs to the dispatch recovery guard and persistent authority
/// registry; a fresh owner resolves or enumerates that registry after stream
/// loss or process restart. This alias intentionally does not add an
/// independent completion handle.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static>>;

/// Closed semantic item emitted by a locator-aware model operation stream.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelStreamItem {
    /// One ordinary structural model event.
    Event(ModelEvent),
    /// A provider recovery locator became durably relevant to this call.
    ///
    /// This is semantic control input for the runtime owner. It must never be
    /// projected as an observational agent event.
    RecoveryLocatorBound(ProviderCallRecoveryLocator),
}

impl From<ModelEvent> for ModelStreamItem {
    fn from(event: ModelEvent) -> Self {
        Self::Event(event)
    }
}

/// Locator-aware semantic model stream used by the runtime recovery path.
pub type ModelControlStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamItem, ModelError>> + Send + 'static>>;

/// An owned future that starts a model stream.
///
/// Consuming [`PreparedModelCall::start`] and the returned future's first poll
/// form one provider-dispatch admission boundary. Implementations must perform
/// no provider-visible work before `start` is called, and must treat a first poll
/// which returns `Pending` as admitted: later polls may finish that same
/// operation, but must not create a second operation. Cancellation after this
/// boundary is cooperative and the future/stream remains joinable.
pub type StartModelCall =
    Pin<Box<dyn Future<Output = Result<ModelStream, ModelError>> + Send + 'static>>;

/// One-shot start future retaining semantic locator-control items.
pub type StartModelCallWithControl =
    Pin<Box<dyn Future<Output = Result<ModelControlStream, ModelError>> + Send + 'static>>;

/// An owned future for a narrow optional model operation.
pub type ModelOperation<T> = Pin<Box<dyn Future<Output = Result<T, ModelError>> + Send + 'static>>;

/// An owned read-only provider-call recovery operation.
///
/// This future may inspect provider-owned evidence for the supplied exact
/// locator. It must never start, recreate, resume, or otherwise mutate a model
/// call, including when the result is ambiguous or unavailable.
pub type ModelCallRecoveryOperation = Pin<
    Box<
        dyn Future<Output = Result<ModelCallRecoveryResult, ModelCallRecoveryError>>
            + Send
            + 'static,
    >,
>;

/// One bounded, credential-free reachability operation.
pub type ReachabilityProbeOperation =
    Pin<Box<dyn Future<Output = Result<(), crate::ModelNetworkFailure>> + Send + 'static>>;

/// A cloneable probe bound to the same immutable route as an executable model.
#[derive(Clone)]
pub struct ReachabilityProbe {
    route: RouteId,
    run: Arc<
        dyn Fn(CancellationToken, std::time::Instant) -> ReachabilityProbeOperation + Send + Sync,
    >,
}

impl fmt::Debug for ReachabilityProbe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReachabilityProbe")
            .field("route", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ReachabilityProbe {
    /// Binds a content-free operation to an exact route identity.
    #[must_use]
    pub fn new(
        route: RouteId,
        run: impl Fn(CancellationToken, std::time::Instant) -> ReachabilityProbeOperation
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            route,
            run: Arc::new(run),
        }
    }

    /// Returns the immutable route key for sharing network readiness observations.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Starts one probe bounded by caller cancellation and an absolute deadline.
    pub fn run(
        &self,
        cancellation: CancellationToken,
        deadline: std::time::Instant,
    ) -> ReachabilityProbeOperation {
        (self.run)(cancellation, deadline)
    }
}

/// Opaque non-cloneable lifetime capability for one admitted model root.
///
/// The runtime retains this value from root admission through terminal
/// publication. Implementations may use `Drop` to close root-scoped authority.
pub trait ModelOperationScope: Send + Sync + 'static {}

struct UnscopedModelOperation;

impl ModelOperationScope for UnscopedModelOperation {}

/// The stable object-safe generation seam used by the agent runtime.
pub trait LanguageModel: Send + Sync + 'static {
    /// Returns a credential-free probe of this model's exact route, if available.
    /// Scripted and custom models without network reachability use `None`.
    fn reachability_probe(&self) -> Option<ReachabilityProbe> {
        None
    }
    /// Host-authored developer instruction for one content-filtered retry.
    ///
    /// Returning `None` disables this retry. The host records the returned
    /// fragment in canonical history before admitting another sampling call.
    fn content_filter_retry_instruction(&self) -> Option<String> {
        None
    }

    /// Claims model-owned authority for one complete admitted logical root.
    ///
    /// The default is inert for models without root-scoped resources. A model
    /// that meters subscription inference must return a nonrenewable scope and
    /// reject duplicate or retired root identities.
    ///
    /// # Errors
    ///
    /// Returns a typed preparation failure when exact root authority cannot be
    /// claimed without renewal or scope drift.
    fn begin_operation_root(
        self: Arc<Self>,
        _root: ModelOperationRoot,
    ) -> Result<Box<dyn ModelOperationScope>, ModelPreparationError> {
        Ok(Box::new(UnscopedModelOperation))
    }

    /// Revalidates the actual selected provider, account, credential authority,
    /// generation, model, and profile without returning secret material.
    ///
    /// The default is deliberately unavailable. A provider adapter must bind
    /// this operation to the same concrete model object and auth owner used for
    /// dispatch; descriptor bytes or caller-copied historical facts are not
    /// fresh authority.
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        _checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async { Err(RuntimeModelAuthorityError::Unavailable) })
    }

    /// Waits until this exact route's credential permits another authority check.
    ///
    /// Only `AwaitingCredential` authorizes this wait. It grants no dispatch
    /// authority: callers must revalidate again afterwards. Dropping the future
    /// cancels the wait, not an independently owned credential refresh.
    /// API-key and custom routes do not wait by default.
    fn wait_for_runtime_authority(self: Arc<Self>) -> RuntimeModelAuthorityWaitOperation {
        Box::pin(async { Err(RuntimeModelAuthorityError::Unavailable) })
    }

    /// Reads provider-owned evidence for one exact previously prepared call.
    ///
    /// The default is deliberately unsupported. Recovery is classification
    /// only: it must not start a new physical call, and `Ambiguous`,
    /// `Unsupported`, or `Unavailable` never authorize redispatch.
    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        Box::pin(async move { ModelCallRecoveryResult::unsupported(&request) })
    }

    /// Lowers one neutral validated plan exactly once and returns its one-shot call.
    ///
    /// # Errors
    ///
    /// Returns typed preparation/evidence unavailability before the start
    /// capability can reach provider-visible work.
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError>;
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
