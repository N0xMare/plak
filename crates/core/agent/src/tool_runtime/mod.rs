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
mod intent;
mod interrupt;
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
use interrupt::{
    cancelled_error, effective_deadline, execution_panic_error, execution_timeout_error,
    wait_for_dispatch_interrupt, wait_for_interrupt,
};
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

    /// Returns who executes the registered tool `name`, when it is registered.
    pub(crate) fn owner(
        &self,
        name: &plakous_model::ToolName,
    ) -> Option<&plakous_tools::ToolOwnerClass> {
        self.registry
            .identity(name)
            .map(RegisteredToolIdentity::owner)
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
    pub fn drain_tasks(&self) -> ToolTaskDrain {
        self.tasks.drain()
    }

    /// Atomically closes task admission, requests abort, and joins to a stable
    /// drained boundary shared by every clone of this runtime.
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
    /// An executor error proven to have had no effect, or a joined stop classified
    /// as `Cancelled`, is a definite `Started` terminal and keeps that error.
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
                if error.proven_no_effect() || error.kind() == ToolRuntimeErrorKind::Cancelled =>
            {
                ToolDispatchReport::started_error(error)
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
        let effect = definition.effect();
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
                        effect,
                    )
                });
                    // Bookkeeping failure deliberately overrides even a proven-no-effect executor error.
                    active
                        .terminalize()
                        .map_err(|error| error.into_portable().with_proven_no_effect(false))?;
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
