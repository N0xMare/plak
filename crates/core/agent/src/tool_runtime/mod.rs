//! Fail-closed deterministic authorization and dispatch owned by the agent driver.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use futures_util::future::{Either, select};
use plakous_model::{
    AttemptId, CallId, Digest, DigestDomain, RunId, Timestamp, ToolEffect, TurnId,
};

mod authority;
mod dispatch_error;
mod execution;
mod report;
mod scheduler;
mod task;

pub use authority::{AuthorizedToolInvocation, EffectAuthorization, PendingEffectDecision};
pub use execution::{
    ToolDispatchObserver, ToolDispatchObserverFuture, ToolDispatchPermit, ToolExecutionControl,
};
pub use report::{ToolDispatchReport, ToolDispatchState};
pub use scheduler::{
    TokioToolTimer, ToolRuntimeConfig, ToolScheduler, ToolSchedulerStatus, ToolTimer,
    ToolTimerFuture,
};
pub use task::{
    TokioToolTaskOwner, ToolTask, ToolTaskDrain, ToolTaskFuture, ToolTaskHandle, ToolTaskJoin,
    ToolTaskOwner, ToolTaskOwnerStatus, ToolTaskStatus,
};

use self::scheduler::{RuntimeLane, SchedulerPermits, default_scheduler};
use authority::{ToolAttemptClaim, ToolAttemptKey, ToolAttemptState, ToolRuntimeAuthority};
use dispatch_error::{DispatchFailure, ToolDispatchError};
use plakous_tools::{
    EffectAuthorizationBinding, EffectAuthorizationGrant, EffectAuthorizationGrantSource,
    EffectDecision, EffectDecisionDisposition, EffectDecisionPrompt, ExecutionEnvironment,
    FrozenToolRegistry, RegisteredToolIdentity, ToolExposureBinding, ToolInvocation,
    ToolInvocationRequest, ToolOutcome, ToolOutcomeLimits, ToolPolicy, ToolPolicyDecision,
    ToolPolicyRevision, ToolRuntimeError, ToolRuntimeErrorKind,
};

const EFFECT_DISPATCH_INTENT_VERSION: u32 = 6;

const MAX_TRACKED_TOOL_ATTEMPTS: usize = 65_536;

/// Exact redaction-safe write-ahead identity for one mutating dispatch.
#[derive(Clone, Eq, PartialEq)]
pub struct EffectDispatchIntent {
    digest: Digest,
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: RegisteredToolIdentity,
    exposure: Option<ToolExposureBinding>,
    argument_digest: Digest,
    environment_digest: Digest,
    policy_revision: ToolPolicyRevision,
    grant_source: EffectAuthorizationGrantSource,
    authorization_digest: Digest,
    effect: ToolEffect,
}

impl EffectDispatchIntent {
    fn for_invocation(
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

/// Non-cloneable authority for one durably claimed mutating invocation.
pub struct ClaimedToolInvocation {
    authorized: AuthorizedToolInvocation,
    intent: EffectDispatchIntent,
    claim_evidence_digest: Digest,
}

struct ClaimedDispatchAuthority {
    intent: EffectDispatchIntent,
    claim_evidence_digest: Digest,
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

/// Provider-neutral executor backed by one immutable registry snapshot.
#[derive(Clone)]
pub struct ToolRuntime {
    registry: FrozenToolRegistry,
    outcome_limits: ToolOutcomeLimits,
    environment: Arc<ExecutionEnvironment>,
    policy: Arc<dyn ToolPolicy>,
    policy_revision: ToolPolicyRevision,
    authority: Arc<ToolRuntimeAuthority>,
    scheduler: ToolScheduler,
    runtime_lane: RuntimeLane,
    timer: Arc<dyn ToolTimer>,
    tasks: Arc<dyn ToolTaskOwner>,
}

struct RuntimeServices {
    scheduler: ToolScheduler,
    config: ToolRuntimeConfig,
    timer: Arc<dyn ToolTimer>,
    tasks: Arc<dyn ToolTaskOwner>,
}

impl Default for RuntimeServices {
    fn default() -> Self {
        Self {
            scheduler: default_scheduler(),
            config: ToolRuntimeConfig::default(),
            timer: Arc::new(TokioToolTimer::new()),
            tasks: Arc::new(TokioToolTaskOwner::new()),
        }
    }
}

/// Drop guard for one entry in the bounded live-attempt table.
///
/// Durable replay protection for mutating work belongs to the caller's effect
/// journal. This process-local authority additionally retains journaled and
/// dispatched attempts as terminal tombstones so the same runtime cannot mint
/// a second dispatch capability.
struct ActiveToolAttempt {
    authority: Arc<ToolRuntimeAuthority>,
    key: ToolAttemptKey,
    binding: Digest,
    state: ToolAttemptState,
    armed: bool,
}

impl ActiveToolAttempt {
    const fn adopt(
        authority: Arc<ToolRuntimeAuthority>,
        key: ToolAttemptKey,
        binding: Digest,
        state: ToolAttemptState,
    ) -> Self {
        Self {
            authority,
            key,
            binding,
            state,
            armed: true,
        }
    }

    fn transition(&mut self, next: ToolAttemptState) -> Result<(), ToolDispatchError> {
        if self
            .authority
            .transition(self.key, self.binding, self.state, next)
        {
            self.state = next;
            Ok(())
        } else {
            Err(authorization_error(
                "tool authorization state transition was rejected",
            ))
        }
    }

    fn release(&mut self) -> Result<(), ToolDispatchError> {
        if self.authority.release(self.key, self.binding, self.state) {
            self.armed = false;
            Ok(())
        } else {
            Err(authorization_error(
                "tool authorization state release was rejected",
            ))
        }
    }

    fn terminalize(&mut self) -> Result<(), ToolDispatchError> {
        self.transition(ToolAttemptState::Terminal)?;
        self.armed = false;
        Ok(())
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveToolAttempt {
    fn drop(&mut self) {
        if self.armed {
            if matches!(
                self.state,
                ToolAttemptState::Journaled | ToolAttemptState::DispatchStarted
            ) {
                let _ = self.authority.transition(
                    self.key,
                    self.binding,
                    self.state,
                    ToolAttemptState::Terminal,
                );
            } else {
                let _ = self.authority.release(self.key, self.binding, self.state);
            }
        }
    }
}

struct DispatchExecution {
    started: bool,
    result: Result<ToolOutcome, DispatchFailure>,
}

impl DispatchExecution {
    fn not_started(error: ToolDispatchError) -> Self {
        Self {
            started: false,
            result: Err(error.into()),
        }
    }

    const fn started(result: Result<ToolOutcome, DispatchFailure>) -> Self {
        Self {
            started: true,
            result,
        }
    }
}

struct ProgressCloseGuard(plakous_tools::ProgressSink);

impl Drop for ProgressCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct DispatchCancellationGuard {
    token: plakous_model::CancellationToken,
    armed: bool,
}

impl DispatchCancellationGuard {
    const fn new(token: plakous_model::CancellationToken) -> Self {
        Self { token, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DispatchCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

impl fmt::Debug for ToolRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRuntime")
            .field("registry", &self.registry)
            .field("outcome_limits", &self.outcome_limits)
            .field("environment", &"<bound>")
            .field("policy_revision", &self.policy_revision)
            .field("authority", &"<bound>")
            .field("scheduler", &self.scheduler)
            .field("timer", &"<bound>")
            .finish_non_exhaustive()
    }
}

impl ToolRuntime {
    /// Creates a runtime over an exact registry, environment, and policy snapshot.
    #[must_use]
    pub fn new(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
    ) -> Self {
        Self::build(
            registry,
            environment,
            policy,
            ToolOutcomeLimits::default(),
            RuntimeServices::default(),
        )
    }

    /// Creates a runtime with immutable authoritative terminal-result limits.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when either limit is zero.
    pub fn with_outcome_limits(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
        outcome_limits: ToolOutcomeLimits,
    ) -> Result<Self, ToolRuntimeError> {
        Ok(Self::build(
            registry,
            environment,
            policy,
            plakous_tools::internal::validate_outcome_limits(outcome_limits)?,
            RuntimeServices::default(),
        ))
    }

    /// Creates a runtime with explicit shared global and per-runtime scheduling budgets.
    #[must_use]
    pub fn with_scheduler(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
        scheduler: ToolScheduler,
        config: ToolRuntimeConfig,
    ) -> Self {
        Self::with_scheduler_and_timer(
            registry,
            environment,
            policy,
            scheduler,
            config,
            Arc::new(TokioToolTimer::new()),
        )
    }

    /// Creates a scheduled runtime with an injected monotonic timer.
    #[must_use]
    pub fn with_scheduler_and_timer(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
        scheduler: ToolScheduler,
        config: ToolRuntimeConfig,
        timer: Arc<dyn ToolTimer>,
    ) -> Self {
        Self::with_scheduler_timer_and_tasks(
            registry,
            environment,
            policy,
            scheduler,
            config,
            timer,
            Arc::new(TokioToolTaskOwner::new()),
        )
    }

    /// Creates a scheduled runtime with injected timer and task ownership.
    #[must_use]
    pub fn with_scheduler_timer_and_tasks(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
        scheduler: ToolScheduler,
        config: ToolRuntimeConfig,
        timer: Arc<dyn ToolTimer>,
        tasks: Arc<dyn ToolTaskOwner>,
    ) -> Self {
        Self::build(
            registry,
            environment,
            policy,
            ToolOutcomeLimits::default(),
            RuntimeServices {
                scheduler,
                config,
                timer,
                tasks,
            },
        )
    }

    fn build(
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        policy: Arc<dyn ToolPolicy>,
        outcome_limits: ToolOutcomeLimits,
        services: RuntimeServices,
    ) -> Self {
        let policy_revision = policy.revision();
        let runtime_lane = services.scheduler.runtime_lane(services.config);
        Self {
            registry,
            outcome_limits,
            environment,
            policy,
            policy_revision,
            authority: Arc::new(ToolRuntimeAuthority::new(MAX_TRACKED_TOOL_ATTEMPTS)),
            scheduler: services.scheduler,
            runtime_lane,
            timer: services.timer,
            tasks: services.tasks,
        }
    }

    /// Returns the immutable model-facing definition snapshot without exposing executors.
    #[must_use]
    pub fn definitions(&self) -> &[plakous_model::ToolDefinition] {
        self.registry.definitions()
    }

    /// Reports whether one exact registered identity belongs to this executable snapshot.
    #[must_use]
    pub(crate) fn owns_registration(&self, identity: &RegisteredToolIdentity) -> bool {
        self.registry.identity(identity.name()) == Some(identity)
    }

    /// Returns a digest of every complete identity in the frozen executable registry.
    ///
    /// Unlike [`Self::definitions`], this binds application registration IDs,
    /// executor identities, and effect-recovery metadata. Durable runtime
    /// witnesses use it to reject recovery through a semantically different
    /// executor allocation even when the model-facing definitions are equal.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error if an identity cannot be represented by
    /// the canonical bounded digest encoding.
    pub fn registry_identity_digest(&self) -> Result<Digest, ToolRuntimeError> {
        self.registry.registry_identity_digest()
    }

    /// Captures reconstruction-complete non-executable registry semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be represented exactly and boundedly.
    pub fn registry_descriptor(
        &self,
    ) -> Result<plakous_tools::FrozenToolRegistryDescriptor, plakous_tools::ToolDescriptorError>
    {
        plakous_tools::FrozenToolRegistryDescriptor::capture(&self.registry)
    }

    /// Returns the immutable limits reapplied to every executor outcome.
    #[must_use]
    pub const fn outcome_limits(&self) -> ToolOutcomeLimits {
        self.outcome_limits
    }

    /// Returns the exact live environment against which grants are validated.
    #[must_use]
    pub const fn environment(&self) -> &Arc<ExecutionEnvironment> {
        &self.environment
    }

    pub(crate) fn existing_joined_process_sessions(
        &self,
    ) -> Option<plakous_tools::JoinedProcessSessions> {
        self.environment.existing_joined_process_sessions()
    }

    /// Returns the immutable policy revision frozen at runtime construction.
    #[must_use]
    pub const fn policy_revision(&self) -> &ToolPolicyRevision {
        &self.policy_revision
    }

    /// Returns this runtime's process scheduler handle.
    #[must_use]
    pub const fn scheduler(&self) -> &ToolScheduler {
        &self.scheduler
    }

    /// Returns the status of dispatched work still owned by this runtime's supervisor.
    #[must_use]
    pub fn task_status(&self) -> ToolTaskOwnerStatus {
        self.tasks.status()
    }

    /// Requests abort for all supervised tasks and returns the number targeted.
    ///
    /// This cannot forcibly preempt a hostile in-process future poll that blocks
    /// its Tokio executor thread; abort takes effect when the runtime regains control.
    #[must_use]
    pub fn request_task_abort(&self) -> usize {
        self.tasks.request_abort_all()
    }

    /// Returns a future completing when the runtime's task owner is drained.
    #[must_use]
    pub fn drain_tasks(&self) -> ToolTaskDrain {
        self.tasks.drain()
    }

    /// Atomically closes task admission, requests abort, and joins to a stable
    /// drained boundary shared by every clone of this runtime.
    #[must_use]
    pub fn shutdown_tasks(&self) -> ToolTaskDrain {
        self.tasks.shutdown()
    }

    /// Strictly validates and identity-binds an invocation without executing it.
    ///
    /// # Errors
    ///
    /// Returns registry or argument validation errors before executor dispatch.
    pub fn prepare(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ToolInvocation, ToolRuntimeError> {
        self.registry.prepare(request)
    }

    /// Evaluates policy for one exact prepared invocation without dispatching it.
    ///
    /// Policy and environment identities are checked both around the asynchronous
    /// evaluation and again immediately before execution. Only an `Authorized`
    /// result contains dispatch authority.
    ///
    /// # Errors
    ///
    /// Returns a policy or snapshot-integrity error. A normal policy denial is a
    /// [`EffectAuthorization::Denied`] value, not an infrastructure error.
    pub async fn authorize(
        &self,
        invocation: ToolInvocation,
    ) -> Result<EffectAuthorization, ToolRuntimeError> {
        self.authorize_control(invocation)
            .await
            .map_err(ToolDispatchError::into_portable)
    }

    /// Restores one exact pre-dispatch decision capability without evaluating policy.
    ///
    /// The caller must reconstruct the invocation from the certified durable
    /// continuation and supply its persisted authorization-binding digest. This
    /// method recomputes the complete current binding (registry, exposure,
    /// executor, arguments, environment, and policy revision) and admits only
    /// exact equality. It creates no authorization grant and performs no dispatch.
    pub(crate) fn restore_pending_effect_decision(
        &self,
        invocation: ToolInvocation,
        binding_digest: Digest,
        prompt: EffectDecisionPrompt,
    ) -> Result<PendingEffectDecision, ToolRuntimeError> {
        self.restore_pending_effect_decision_control(invocation, binding_digest, prompt)
            .map_err(ToolDispatchError::into_portable)
    }

    fn restore_pending_effect_decision_control(
        &self,
        invocation: ToolInvocation,
        binding_digest: Digest,
        prompt: EffectDecisionPrompt,
    ) -> Result<PendingEffectDecision, ToolDispatchError> {
        self.verify_invocation(&invocation)?;
        let binding = self.binding_for(&invocation)?;
        if binding.digest() != binding_digest {
            return Err(authorization_error(
                "persisted tool authorization binding is not exact",
            ));
        }
        self.verify_binding(&invocation, &binding)?;
        let attempt_key = ToolAttemptKey::for_invocation(&invocation);
        match self
            .authority
            .restore_pending(attempt_key, binding.digest())
        {
            ToolAttemptClaim::Claimed => Ok(PendingEffectDecision::new(
                invocation,
                binding,
                prompt,
                Arc::clone(&self.authority),
            )),
            ToolAttemptClaim::Duplicate => Err(authorization_error(
                "pending tool decision was already restored by this runtime",
            )),
            ToolAttemptClaim::CapacityExceeded => Err(ToolDispatchError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool runtime attempt capacity was exhausted",
            )),
        }
    }

    async fn authorize_control(
        &self,
        invocation: ToolInvocation,
    ) -> Result<EffectAuthorization, ToolDispatchError> {
        self.verify_invocation(&invocation)?;
        let binding = self.binding_for(&invocation)?;
        let attempt_key = ToolAttemptKey::for_invocation(&invocation);
        match self.authority.begin(attempt_key, binding.digest()) {
            ToolAttemptClaim::Claimed => {}
            ToolAttemptClaim::Duplicate => {
                return Err(authorization_error(
                    "tool invocation was already admitted by this runtime",
                ));
            }
            ToolAttemptClaim::CapacityExceeded => {
                return Err(ToolDispatchError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "tool runtime attempt capacity was exhausted",
                ));
            }
        }
        let mut active = ActiveToolAttempt::adopt(
            Arc::clone(&self.authority),
            attempt_key,
            binding.digest(),
            ToolAttemptState::Evaluating,
        );
        let definition = self
            .registry
            .definition(invocation.identity().name())
            .cloned()
            .ok_or_else(|| authorization_error("prepared tool definition is unavailable"))?;
        let request = plakous_tools::internal::policy_request(
            invocation.clone(),
            definition,
            Arc::clone(&self.environment),
            self.policy_revision.clone(),
            binding.clone(),
        );
        let decision = self
            .policy
            .evaluate(request)
            .await
            .map_err(ToolDispatchError::from_control)?;
        self.verify_binding(&invocation, &binding)?;
        Ok(match decision {
            ToolPolicyDecision::Allowed => {
                active.transition(ToolAttemptState::Authorized)?;
                active.disarm();
                let grant = plakous_tools::internal::authorization_grant(
                    binding,
                    EffectAuthorizationGrantSource::Policy,
                );
                EffectAuthorization::Allowed(AuthorizedToolInvocation::new(
                    invocation,
                    grant,
                    Arc::clone(&self.authority),
                    self.registry.clone(),
                    Arc::clone(&self.environment),
                ))
            }
            ToolPolicyDecision::Denied(denial) => {
                active.release()?;
                EffectAuthorization::Denied(denial)
            }
            ToolPolicyDecision::DecisionRequired(prompt) => {
                active.transition(ToolAttemptState::DecisionPending)?;
                active.disarm();
                EffectAuthorization::DecisionRequired(PendingEffectDecision::new(
                    invocation,
                    binding,
                    prompt,
                    Arc::clone(&self.authority),
                ))
            }
            _ => {
                active.release()?;
                return Err(authorization_error("unsupported tool policy decision"));
            }
        })
    }

    /// Resolves one pending decision after revalidating every authoritative fact.
    ///
    /// # Errors
    ///
    /// Returns a policy error if the pending request came from a different
    /// runtime clone set or if its registry, environment, or policy binding is
    /// no longer exact.
    pub fn resolve_effect_decision(
        &self,
        pending: PendingEffectDecision,
        decision: EffectDecision,
    ) -> Result<EffectAuthorization, ToolRuntimeError> {
        self.resolve_effect_decision_control(pending, decision)
            .map_err(ToolDispatchError::into_portable)
    }

    fn resolve_effect_decision_control(
        &self,
        pending: PendingEffectDecision,
        decision: EffectDecision,
    ) -> Result<EffectAuthorization, ToolDispatchError> {
        if !pending.belongs_to(&self.authority) {
            return Err(authorization_error(
                "pending decision belongs to a different tool runtime",
            ));
        }
        if decision.binding_digest() != pending.binding().digest() {
            return Err(authorization_error(
                "effect-decision response names a different tool binding",
            ));
        }
        self.verify_binding(pending.invocation(), pending.binding())?;
        let (invocation, binding, authority) = pending.into_parts();
        let attempt_key = ToolAttemptKey::for_invocation(&invocation);
        let mut active = ActiveToolAttempt::adopt(
            authority,
            attempt_key,
            binding.digest(),
            ToolAttemptState::DecisionPending,
        );
        Ok(
            match plakous_tools::internal::effect_decision_disposition(decision) {
                EffectDecisionDisposition::Allow(decision_audit_id) => {
                    active.transition(ToolAttemptState::Authorized)?;
                    active.disarm();
                    let grant = plakous_tools::internal::authorization_grant(
                        binding,
                        EffectAuthorizationGrantSource::Operator(decision_audit_id),
                    );
                    EffectAuthorization::Allowed(AuthorizedToolInvocation::new(
                        invocation,
                        grant,
                        Arc::clone(&self.authority),
                        self.registry.clone(),
                        Arc::clone(&self.environment),
                    ))
                }
                EffectDecisionDisposition::Deny(denial) => {
                    active.release()?;
                    EffectAuthorization::Denied(denial)
                }
                _ => {
                    active.release()?;
                    return Err(authorization_error(
                        "unsupported tool effect-decision disposition",
                    ));
                }
            },
        )
    }

    /// Derives the exact redaction-safe intent that must be journaled for a mutation.
    ///
    /// # Errors
    ///
    /// Returns an effect-safety error unless the capability is bound to this
    /// runtime and the registered tool is mutating. Local tool recovery is the
    /// fixed manual-no-key policy; executors supply no recovery metadata.
    pub fn effect_dispatch_intent(
        &self,
        authorized: &AuthorizedToolInvocation,
    ) -> Result<EffectDispatchIntent, ToolRuntimeError> {
        self.effect_dispatch_intent_control(authorized)
            .map_err(ToolDispatchError::into_portable)
    }

    fn effect_dispatch_intent_control(
        &self,
        authorized: &AuthorizedToolInvocation,
    ) -> Result<EffectDispatchIntent, ToolDispatchError> {
        if !authorized.matches_live_fence(&self.authority, &self.registry, &self.environment) {
            return Err(authorization_error(
                "authorized invocation belongs to a different tool runtime",
            ));
        }
        self.effect_dispatch_intent_for(authorized.invocation(), authorized.grant())
    }

    fn effect_dispatch_intent_for(
        &self,
        invocation: &ToolInvocation,
        grant: &EffectAuthorizationGrant,
    ) -> Result<EffectDispatchIntent, ToolDispatchError> {
        self.verify_binding(invocation, grant.binding())?;
        let definition = self
            .registry
            .definition(invocation.identity().name())
            .ok_or_else(|| authorization_error("prepared tool definition is unavailable"))?;
        if definition.effect() != ToolEffect::Mutating {
            return Err(effect_safety_error(
                "effect dispatch intent requires a mutating tool",
            ));
        }
        EffectDispatchIntent::for_invocation(invocation, grant)
    }

    /// Consumes one exact durably claimed mutation and mints dispatch authority.
    ///
    /// The caller must persist the claim before calling this method. The runtime
    /// rederives and compares every immutable intent fact and retains the exact
    /// persisted component digest delivered by that internal authority owner.
    /// The executor cannot be reached by this method.
    ///
    /// # Errors
    ///
    /// Returns a pre-dispatch error when the supplied claim disagrees with the
    /// authorized invocation.
    pub fn claim_effect(
        &self,
        authorized: AuthorizedToolInvocation,
        intent: EffectDispatchIntent,
        claim_evidence_digest: Digest,
    ) -> Result<ClaimedToolInvocation, ToolRuntimeError> {
        self.claim_effect_control(authorized, intent, claim_evidence_digest)
            .map_err(ToolDispatchError::into_portable)
    }

    fn claim_effect_control(
        &self,
        mut authorized: AuthorizedToolInvocation,
        intent: EffectDispatchIntent,
        claim_evidence_digest: Digest,
    ) -> Result<ClaimedToolInvocation, ToolDispatchError> {
        let current = self.effect_dispatch_intent_control(&authorized)?;
        if current != intent {
            return Err(effect_safety_error(
                "durable effect claim does not match authoritative invocation facts",
            ));
        }
        if !authorized.mark_journaled() {
            return Err(effect_safety_error(
                "claimed tool authorization state transition was rejected",
            ));
        }
        Ok(ClaimedToolInvocation {
            authorized,
            intent,
            claim_evidence_digest,
        })
    }

    /// Consumes one opaque authorized invocation and dispatches it once.
    ///
    /// The same token remains observable inside the tool through
    /// [`ToolContext`](plakous_tools::ToolContext). When cancellation wins, the runtime waits for the owned
    /// executor future to finish its cleanup handshake before acknowledging
    /// cancellation. Progress is closed on every terminal path.
    ///
    /// # Errors
    ///
    /// Returns cancellation, snapshot-integrity, or executor errors. A tool's
    /// model-visible failure remains an `Ok(ToolOutcome)`.
    #[allow(
        clippy::too_many_lines,
        reason = "keeping the complete authorization-to-dispatch sequence together makes the safety boundary auditable"
    )]
    pub async fn execute(
        &self,
        authorized: AuthorizedToolInvocation,
        control: ToolExecutionControl,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        if !authorized.matches_live_fence(&self.authority, &self.registry, &self.environment) {
            return Err(authorization_error(
                "authorized invocation belongs to a different tool runtime",
            )
            .into_portable());
        }
        self.verify_binding(authorized.invocation(), authorized.grant().binding())
            .map_err(ToolDispatchError::into_portable)?;
        let definition = self
            .registry
            .definition(authorized.invocation().identity().name())
            .ok_or_else(|| authorization_error("prepared tool definition is unavailable"))
            .map_err(ToolDispatchError::into_portable)?;
        if definition.effect() != ToolEffect::ReadOnly {
            return Err(
                authorization_error("effectful tool dispatch requires a durable claim")
                    .into_portable(),
            );
        }
        self.dispatch_authorized(authorized, control, ToolAttemptState::Authorized, None)
            .await
            .result
            .map_err(DispatchFailure::into_portable)
    }

    /// Consumes one successfully claimed capability and dispatches it once.
    ///
    /// Every failure before executor invocation is reported as `NotStarted`.
    /// Once invocation begins, loss of an authoritative outcome is conservatively
    /// reported as `Started` with `OutcomeUnknown` and its safe uncertainty cause.
    /// An executor that joins cancellation and classifies its own stop as
    /// `Cancelled` is a definite `Started` terminal and keeps that error.
    pub async fn execute_claimed(
        &self,
        claimed: ClaimedToolInvocation,
        control: ToolExecutionControl,
    ) -> ToolDispatchReport {
        let current = match self.effect_dispatch_intent_control(&claimed.authorized) {
            Ok(intent) => intent,
            Err(error) => return ToolDispatchReport::not_started(error.into_portable()),
        };
        if current != claimed.intent {
            return ToolDispatchReport::not_started(
                effect_safety_error(
                    "claimed effect no longer matches authoritative dispatch facts",
                )
                .into_portable(),
            );
        }
        let effect_authority = Some(ClaimedDispatchAuthority {
            intent: claimed.intent,
            claim_evidence_digest: claimed.claim_evidence_digest,
        });
        let dispatch = self
            .dispatch_authorized(
                claimed.authorized,
                control,
                ToolAttemptState::Journaled,
                effect_authority,
            )
            .await;
        match (dispatch.started, dispatch.result) {
            (false, Err(error)) => ToolDispatchReport::not_started(error.into_portable()),
            (true, Ok(outcome)) => ToolDispatchReport::started(outcome),
            (true, Err(DispatchFailure::Executor(error)))
                if error.kind() == ToolRuntimeErrorKind::Cancelled =>
            {
                ToolDispatchReport::started_cancelled(error)
            }
            (true, Err(error)) => ToolDispatchReport::started_unknown(error.into_portable()),
            (false, Ok(_)) => ToolDispatchReport::not_started(
                effect_safety_error("tool dispatch reported an invalid pre-dispatch success")
                    .into_portable(),
            ),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping the complete authorization-to-dispatch sequence together makes the safety boundary auditable"
    )]
    async fn dispatch_authorized(
        &self,
        authorized: AuthorizedToolInvocation,
        control: ToolExecutionControl,
        initial_state: ToolAttemptState,
        effect_authority: Option<ClaimedDispatchAuthority>,
    ) -> DispatchExecution {
        let progress = control.progress().clone();
        let progress_guard = ProgressCloseGuard(progress.clone());
        if !authorized.matches_live_fence(&self.authority, &self.registry, &self.environment) {
            return DispatchExecution::not_started(authorization_error(
                "authorized invocation belongs to a different tool runtime",
            ));
        }
        if let Err(error) =
            self.verify_binding(authorized.invocation(), authorized.grant().binding())
        {
            return DispatchExecution::not_started(error);
        }
        if authorized.invocation().call_id() != progress.call_id() {
            return DispatchExecution::not_started(ToolDispatchError::new(
                ToolRuntimeErrorKind::Execution,
                "tool controls do not match the prepared invocation",
            ));
        }
        if control.cancellation().is_cancelled() {
            return DispatchExecution::not_started(ToolDispatchError::new(
                ToolRuntimeErrorKind::Cancelled,
                "tool execution was cancelled",
            ));
        }
        let Some(definition) = self
            .registry
            .definition(authorized.invocation().identity().name())
        else {
            return DispatchExecution::not_started(authorization_error(
                "prepared tool definition is unavailable",
            ));
        };
        let output_contract = definition.output().clone();
        let concurrency = definition.concurrency();
        // A foreground process call may legitimately run as long as a joined
        // process session (a 20-minute build), so one dispatch is bounded by
        // the session duration, not the 300-second literal-process duration.
        let deadline = effective_deadline(
            control.clock().now(),
            control.deadline(),
            self.environment.config().limits().session_duration(),
        );
        let (mut invocation, grant, authority) = authorized.into_parts();
        let attempt_key = ToolAttemptKey::for_invocation(&invocation);
        let binding_digest = grant.binding().digest();
        let mut active =
            ActiveToolAttempt::adopt(authority, attempt_key, binding_digest, initial_state);
        let acquisition =
            SchedulerPermits::acquire(&self.scheduler, &self.runtime_lane, &concurrency);
        let interrupt = wait_for_interrupt(
            control.cancellation().clone(),
            deadline.monotonic,
            Arc::clone(&self.timer),
        );
        let permits = match select(Box::pin(acquisition), Box::pin(interrupt)).await {
            Either::Left((Ok(permits), _)) => permits,
            Either::Left((Err(error), _)) => return DispatchExecution::not_started(error),
            Either::Right((Interrupt::Cancelled, _)) => {
                return DispatchExecution::not_started(cancelled_error());
            }
            Either::Right((Interrupt::Deadline, _)) => {
                return DispatchExecution::not_started(execution_timeout_error());
            }
        };
        if control.cancellation().is_cancelled() {
            return DispatchExecution::not_started(cancelled_error());
        }
        if Instant::now() >= deadline.monotonic {
            return DispatchExecution::not_started(execution_timeout_error());
        }
        if let Err(error) = active.transition(ToolAttemptState::DispatchStarted) {
            return DispatchExecution::not_started(error);
        }
        let dispatch_permit = if let Some(observer) = control.dispatch_observer() {
            let Ok(notification) = catch_unwind(AssertUnwindSafe(|| observer.dispatch_started()))
            else {
                return DispatchExecution::not_started(dispatch_observer_error());
            };
            let notification =
                AssertUnwindSafe(notification)
                    .catch_unwind()
                    .map(|result| match result {
                        Ok(result) => result.map_err(ToolDispatchError::from_control),
                        Err(_) => Err(dispatch_observer_error()),
                    });
            let interrupt = wait_for_interrupt(
                control.cancellation().clone(),
                deadline.monotonic,
                Arc::clone(&self.timer),
            );
            match select(Box::pin(notification), Box::pin(interrupt)).await {
                Either::Left((Ok(permit), _)) => Some(permit),
                Either::Left((Err(error), _)) => {
                    return DispatchExecution::not_started(error);
                }
                Either::Right((Interrupt::Cancelled, _)) => {
                    return DispatchExecution::not_started(cancelled_error());
                }
                Either::Right((Interrupt::Deadline, _)) => {
                    return DispatchExecution::not_started(execution_timeout_error());
                }
            }
        } else {
            None
        };
        // Admission may wait behind other calls. Revalidate every mutable
        // authoritative dependency at the dispatch linearization point.
        if let Err(error) = self.verify_binding(&invocation, grant.binding()) {
            return DispatchExecution::not_started(error);
        }
        if let Some(authority) = &effect_authority {
            let current = match self.effect_dispatch_intent_for(&invocation, &grant) {
                Ok(intent) => intent,
                Err(error) => return DispatchExecution::not_started(error),
            };
            if current != authority.intent {
                return DispatchExecution::not_started(effect_safety_error(
                    "durable effect claim failed exact dispatch-time revalidation",
                ));
            }
        }
        // The observer can wait on durable admission. Recheck cooperative
        // controls after that final wait before constructing the executor.
        if control.cancellation().is_cancelled() {
            return DispatchExecution::not_started(cancelled_error());
        }
        if Instant::now() >= deadline.monotonic {
            return DispatchExecution::not_started(execution_timeout_error());
        }
        if let Some(permit) = dispatch_permit {
            match catch_unwind(AssertUnwindSafe(|| permit.start())) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return DispatchExecution::not_started(ToolDispatchError::from_control(error));
                }
                Err(_) => return DispatchExecution::not_started(dispatch_observer_error()),
            }
        }
        if let Some(authority) = effect_authority {
            invocation = plakous_tools::internal::bind_effect_dispatch(
                invocation,
                authority.intent.digest(),
                authority.claim_evidence_digest,
            );
        }
        let result = async {
            let execution_cancellation = plakous_model::CancellationToken::new();
            let context = plakous_tools::internal::tool_context(
                invocation.run_id(),
                invocation.turn_id(),
                invocation.attempt_id(),
                execution_cancellation.clone(),
                Some(deadline.wall),
                Arc::clone(control.clock()),
                Arc::clone(&self.environment),
                grant,
                progress.clone(),
            )
            .with_preempt(control.preempt().clone());
            let executor = plakous_tools::internal::executor(&self.registry, &invocation)
                .map_err(ToolDispatchError::from_control)?;
            let caller_cancellation = control.cancellation().clone();
            let execution =
                catch_unwind(AssertUnwindSafe(|| executor.execute(invocation, context)))
                    .map_err(|_| execution_panic_error())?;
            let mut cancellation_guard =
                DispatchCancellationGuard::new(execution_cancellation.clone());
            let outcome_limits = self.outcome_limits;
            let timer = Arc::clone(&self.timer);
            let cleanup_duration = self.environment.config().limits().cleanup_duration();
            let task = self
                .tasks
                .spawn(Box::pin(async move {
                    let _permits = permits;
                    let _progress_guard = progress_guard;
                    let interrupt = wait_for_dispatch_interrupt(
                        caller_cancellation,
                        execution_cancellation.clone(),
                        deadline.monotonic,
                        Arc::clone(&timer),
                    );
                    let result = match select(execution, Box::pin(interrupt)).await {
                Either::Left((result, _)) => result,
                Either::Right((reason, execution)) => {
                    execution_cancellation.cancel();
                    let cleanup = timer.sleep_until(Instant::now() + cleanup_duration);
                    match select(execution, Box::pin(cleanup)).await {
                        // Once dispatch starts, the executor's terminal
                        // classification is authoritative even after an interrupt.
                        Either::Left((result, _)) => match (reason, result) {
                            (Interrupt::Deadline, Err(error))
                                if error.kind() == ToolRuntimeErrorKind::Cancelled =>
                            {
                                Err(execution_timeout_error().into_portable())
                            }
                            (_, result) => result,
                        },
                        Either::Right(((), execution)) => {
                            // Dropping an async executor future ends that in-process
                            // work before its permits are released. A poll that never
                            // returns also prevents this timer from being observed.
                            drop(execution);
                            Err(ToolRuntimeError::new(
                                ToolRuntimeErrorKind::CleanupTimeout,
                                "tool cleanup exceeded its grace period; effect safety is unknown",
                            ))
                        }
                    }
                }
            }
                .and_then(|outcome| {
                    plakous_tools::internal::enforce_outcome_limits(outcome, outcome_limits)
                })
                .map(|outcome| {
                    plakous_tools::internal::enforce_completed_output_contract(
                        outcome,
                        &output_contract,
                    )
                });
                    active
                        .terminalize()
                        .map_err(ToolDispatchError::into_portable)?;
                    result
                }))
                .map_err(ToolDispatchError::from_control)?;
            let result = task.join().await;
            cancellation_guard.disarm();
            result.map_err(DispatchFailure::Executor)
        }
        .await;
        DispatchExecution::started(result)
    }

    fn binding_for(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<EffectAuthorizationBinding, ToolDispatchError> {
        plakous_tools::internal::authorization_binding(
            invocation,
            self.environment.descriptor().clone(),
            self.policy_revision.clone(),
        )
        .map_err(ToolDispatchError::from_control)
    }

    fn verify_invocation(&self, invocation: &ToolInvocation) -> Result<(), ToolDispatchError> {
        self.verify_policy_revision()?;
        let scope = self.environment.descriptor().scope();
        if scope.run_id() != invocation.run_id()
            || scope
                .attempt_id()
                .is_some_and(|attempt_id| attempt_id != invocation.attempt_id())
        {
            return Err(authorization_error(
                "execution environment scope does not cover the tool invocation",
            ));
        }
        self.environment
            .verify_descriptor(self.environment.descriptor())
            .map_err(|_error| {
                ToolDispatchError::new(
                    ToolRuntimeErrorKind::Policy,
                    "execution environment identity is no longer valid",
                )
            })?;
        plakous_tools::internal::executor(&self.registry, invocation)
            .map(|_| ())
            .map_err(ToolDispatchError::from_control)
    }

    fn verify_binding(
        &self,
        invocation: &ToolInvocation,
        binding: &EffectAuthorizationBinding,
    ) -> Result<(), ToolDispatchError> {
        self.verify_invocation(invocation)?;
        self.environment
            .verify_descriptor(binding.environment_descriptor())
            .map_err(|_error| {
                ToolDispatchError::new(
                    ToolRuntimeErrorKind::Policy,
                    "decision environment binding is no longer valid",
                )
            })?;
        let expected = self.binding_for(invocation)?;
        if expected == *binding {
            Ok(())
        } else {
            Err(authorization_error(
                "tool authorization binding does not match the invocation",
            ))
        }
    }

    fn verify_policy_revision(&self) -> Result<(), ToolDispatchError> {
        if self.policy.revision() == self.policy_revision {
            Ok(())
        } else {
            Err(authorization_error(
                "tool policy revision changed after runtime construction",
            ))
        }
    }
}

fn authorization_error(message: &'static str) -> ToolDispatchError {
    ToolDispatchError::new(ToolRuntimeErrorKind::Policy, message)
}

fn effect_safety_error(message: &'static str) -> ToolDispatchError {
    ToolDispatchError::new(ToolRuntimeErrorKind::EffectSafety, message)
}

fn dispatch_observer_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Execution,
        "tool dispatch observer failed before executor start",
    )
}

#[derive(Clone, Copy)]
struct EffectiveDeadline {
    wall: Timestamp,
    monotonic: Instant,
}

#[derive(Clone, Copy)]
enum Interrupt {
    Cancelled,
    Deadline,
}

async fn wait_for_interrupt(
    cancellation: plakous_model::CancellationToken,
    deadline: Instant,
    timer: Arc<dyn ToolTimer>,
) -> Interrupt {
    if cancellation.is_cancelled() {
        return Interrupt::Cancelled;
    }
    match select(cancellation.cancelled(), timer.sleep_until(deadline)).await {
        Either::Left(((), _)) => Interrupt::Cancelled,
        Either::Right(((), _)) => Interrupt::Deadline,
    }
}

async fn wait_for_dispatch_interrupt(
    caller_cancellation: plakous_model::CancellationToken,
    execution_cancellation: plakous_model::CancellationToken,
    deadline: Instant,
    timer: Arc<dyn ToolTimer>,
) -> Interrupt {
    if caller_cancellation.is_cancelled() || execution_cancellation.is_cancelled() {
        return Interrupt::Cancelled;
    }
    let either_cancelled = select(
        caller_cancellation.cancelled(),
        execution_cancellation.cancelled(),
    );
    match select(Box::pin(either_cancelled), timer.sleep_until(deadline)).await {
        Either::Left(_) => Interrupt::Cancelled,
        Either::Right(_) => Interrupt::Deadline,
    }
}

fn effective_deadline(
    now: Timestamp,
    caller: Option<Timestamp>,
    execution_duration: Duration,
) -> EffectiveDeadline {
    let environment = timestamp_add(now, execution_duration);
    let wall = caller.map_or(environment, |caller| caller.min(environment));
    let remaining = timestamp_duration_since(wall, now);
    EffectiveDeadline {
        wall,
        monotonic: Instant::now() + remaining,
    }
}

fn timestamp_add(timestamp: Timestamp, duration: Duration) -> Timestamp {
    let nanos = u64::from(timestamp.nanoseconds()) + u64::from(duration.subsec_nanos());
    let carry = nanos / 1_000_000_000;
    let seconds = timestamp
        .seconds()
        .saturating_add(duration.as_secs())
        .saturating_add(carry);
    let nanoseconds = if seconds == u64::MAX {
        999_999_999
    } else {
        (nanos % 1_000_000_000) as u32
    };
    match Timestamp::new(seconds, nanoseconds) {
        Ok(timestamp) => timestamp,
        Err(_) => timestamp,
    }
}

fn timestamp_duration_since(later: Timestamp, earlier: Timestamp) -> Duration {
    if later <= earlier {
        return Duration::ZERO;
    }
    let mut seconds = later.seconds() - earlier.seconds();
    let nanoseconds = if later.nanoseconds() >= earlier.nanoseconds() {
        later.nanoseconds() - earlier.nanoseconds()
    } else {
        seconds -= 1;
        1_000_000_000 + later.nanoseconds() - earlier.nanoseconds()
    };
    Duration::new(seconds, nanoseconds)
}

fn cancelled_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Cancelled,
        "tool execution was cancelled",
    )
}

fn execution_timeout_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::ExecutionTimeout,
        "tool execution deadline elapsed before dispatch completed",
    )
}

fn execution_panic_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Execution,
        "tool executor panicked during dispatch",
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use plakous_model::{
        DigestDomain, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolInputContract,
        ToolName, ToolOutputContract,
    };
    use serde_json::json;

    use super::*;
    use plakous_tools::{
        EFFECT_AUTHORIZATION_BINDING_VERSION, EffectDecisionAuditId, EffectDecisionPrompt,
        ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor, ExecutionEnvironmentScope,
        RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolFuture,
        ToolInvocationRequest, ToolOutcome, ToolPolicyFuture, ToolRegistration,
        ToolRegistryBuilder,
    };

    struct FixedTool(ToolOutcome);

    struct CountingPolicy {
        evaluations: Arc<AtomicUsize>,
        revision: ToolPolicyRevision,
    }

    impl ToolPolicy for CountingPolicy {
        fn revision(&self) -> ToolPolicyRevision {
            self.revision.clone()
        }

        fn evaluate(&self, _request: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
            self.evaluations.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ToolPolicyDecision::Allowed) })
        }
    }

    impl ToolExecutor for FixedTool {
        fn execute(
            self: Arc<Self>,
            _invocation: ToolInvocation,
            _context: ToolContext,
        ) -> ToolFuture {
            let outcome = self.0.clone();
            Box::pin(async move { Ok(outcome) })
        }
    }

    fn environment_descriptor(
        run_id: RunId,
    ) -> Result<ExecutionEnvironmentDescriptor, Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run_id),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        Ok(environment.descriptor().clone())
    }

    #[test]
    fn authorization_and_effect_intent_formats_bind_environment_descriptor()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = ToolDefinition::new(
            ToolName::new("mutate")?,
            "1.0.0",
            "Mutates a fixed test resource.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?;
        let tool = Arc::new(FixedTool(ToolOutcome::success_text("ok")?));
        let mut builder = ToolRegistryBuilder::default();
        builder.register(ToolRegistration::new(
            RegisteredToolId::new("test.mutate")?,
            ToolExecutorId::new("test.mutate.v1")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.mutate.v1"])?,
            definition,
            tool,
        ))?;
        let run_id = RunId::from_str("00000000-0000-0000-0000-000000000001")?;
        let invocation = builder.freeze().prepare(ToolInvocationRequest::new(
            run_id,
            TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
            CallId::from_str("00000000-0000-0000-0000-000000000004")?,
            ToolName::new("mutate")?,
            ToolCorrelation::default(),
            Arc::<str>::from(r#"{"value":"fixed"}"#),
        ))?;
        let binding = plakous_tools::internal::authorization_binding(
            &invocation,
            environment_descriptor(run_id)?,
            ToolPolicyRevision::new("test.fixed-policy", 7)?,
        )?;
        assert_eq!(binding.digest().domain(), DigestDomain::EffectAuthorization);
        assert_eq!(
            binding.digest().version(),
            EFFECT_AUTHORIZATION_BINDING_VERSION
        );
        assert_eq!(binding.environment_descriptor().scope().run_id(), run_id);

        let grant = plakous_tools::internal::authorization_grant(
            binding,
            EffectAuthorizationGrantSource::Policy,
        );
        let intent = EffectDispatchIntent::for_invocation(&invocation, &grant)?;
        assert_eq!(intent.digest().domain(), DigestDomain::EffectAuthorization);
        assert_eq!(intent.digest().version(), EFFECT_DISPATCH_INTENT_VERSION);
        Ok(())
    }

    #[test]
    fn exact_pending_restore_skips_policy_and_rejects_replay_or_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let definition = ToolDefinition::new(
            ToolName::new("read")?,
            "1.0.0",
            "Reads a fixed test resource.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        let tool = Arc::new(FixedTool(ToolOutcome::success_text("ok")?));
        let mut builder = ToolRegistryBuilder::default();
        builder.register(ToolRegistration::new(
            RegisteredToolId::new("test.read")?,
            ToolExecutorId::new("test.read.v1")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.read.v1"])?,
            definition,
            tool,
        ))?;
        let run_id = RunId::from_str("00000000-0000-0000-0000-000000000011")?;
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run_id),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let evaluations = Arc::new(AtomicUsize::new(0));
        let revision = ToolPolicyRevision::new("test.restore-policy", 1)?;
        let runtime = ToolRuntime::new(
            builder.freeze(),
            Arc::clone(&environment),
            Arc::new(CountingPolicy {
                evaluations: Arc::clone(&evaluations),
                revision: revision.clone(),
            }),
        );
        let request = ToolInvocationRequest::new(
            run_id,
            TurnId::from_str("00000000-0000-0000-0000-000000000012")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000013")?,
            CallId::from_str("00000000-0000-0000-0000-000000000014")?,
            ToolName::new("read")?,
            ToolCorrelation::default(),
            Arc::<str>::from(r#"{"value":"fixed"}"#),
        );
        let invocation = runtime.prepare(request.clone())?;
        let binding = plakous_tools::internal::authorization_binding(
            &invocation,
            environment.descriptor().clone(),
            revision,
        )?;
        let pending = runtime.restore_pending_effect_decision(
            invocation,
            binding.digest(),
            EffectDecisionPrompt::new("restore"),
        )?;
        assert_eq!(pending.invocation().turn_id(), request.turn_id());
        assert_eq!(evaluations.load(Ordering::SeqCst), 0);

        let duplicate = runtime.restore_pending_effect_decision(
            runtime.prepare(request.clone())?,
            binding.digest(),
            EffectDecisionPrompt::new("restore"),
        );
        assert!(duplicate.is_err());

        let resolved = runtime.resolve_effect_decision(
            pending,
            EffectDecision::allow(
                binding.digest(),
                EffectDecisionAuditId::new("restore:allow")?,
            ),
        )?;
        assert!(matches!(resolved, EffectAuthorization::Allowed(_)));
        assert_eq!(evaluations.load(Ordering::SeqCst), 0);

        let deny_request = ToolInvocationRequest::new(
            run_id,
            request.turn_id(),
            request.attempt_id(),
            CallId::from_str("00000000-0000-0000-0000-000000000015")?,
            ToolName::new("read")?,
            ToolCorrelation::default(),
            Arc::<str>::from(r#"{"value":"deny"}"#),
        );
        let deny_invocation = runtime.prepare(deny_request)?;
        let deny_binding = plakous_tools::internal::authorization_binding(
            &deny_invocation,
            environment.descriptor().clone(),
            runtime.policy_revision().clone(),
        )?;
        let deny_pending = runtime.restore_pending_effect_decision(
            deny_invocation,
            deny_binding.digest(),
            EffectDecisionPrompt::new("restore deny"),
        )?;
        let denied = runtime.resolve_effect_decision(
            deny_pending,
            EffectDecision::deny(
                deny_binding.digest(),
                plakous_tools::ToolPolicyDenial::new("operator denied"),
            ),
        )?;
        assert!(matches!(denied, EffectAuthorization::Denied(_)));
        assert_eq!(evaluations.load(Ordering::SeqCst), 0);

        let drifted = runtime.restore_pending_effect_decision(
            runtime.prepare(request)?,
            Digest::sha256(DigestDomain::EffectAuthorization, 6, &[b"drift"])?,
            EffectDecisionPrompt::new("restore"),
        );
        assert!(drifted.is_err());
        assert_eq!(evaluations.load(Ordering::SeqCst), 0);
        Ok(())
    }
}
