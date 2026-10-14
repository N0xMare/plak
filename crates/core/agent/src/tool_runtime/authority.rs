//! Agent-owned admitted invocation and replay-prevention capabilities.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use plakous_model::{AttemptId, CallId, Digest, RunId, TurnId};
use plakous_tools::{
    EffectAuthorizationBinding, EffectAuthorizationGrant, EffectDecisionPrompt,
    ExecutionEnvironment, FrozenToolRegistry, ToolInvocation, ToolPolicyDenial,
};

/// Private in-process authority shared only by clones of one [`crate::ToolRuntime`].
///
/// Digest equality is necessary for audit and persistence, but dispatch also
/// requires this unforgeable process-local capability. Deserialized or
/// independently reconstructed values therefore cannot become executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolAttemptState {
    Evaluating,
    DecisionPending,
    Authorized,
    Journaled,
    DispatchStarted,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolAttemptClaim {
    Claimed,
    Duplicate,
    CapacityExceeded,
}

/// Canonical identity of one logical tool-call attempt.
///
/// The full authorization binding is deliberately not the map key: changing the
/// tool, arguments, exposure, environment, or policy under these same IDs must
/// be rejected as a conflicting replay rather than admitted as another call.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ToolAttemptKey {
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    call: CallId,
}

impl ToolAttemptKey {
    pub(crate) const fn for_invocation(invocation: &ToolInvocation) -> Self {
        Self {
            run: invocation.run_id(),
            turn: invocation.turn_id(),
            attempt: invocation.attempt_id(),
            call: invocation.call_id(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolAttemptRecord {
    binding: Digest,
    state: ToolAttemptState,
}

#[derive(Debug)]
pub(crate) struct ToolRuntimeAuthority {
    attempts: Mutex<HashMap<ToolAttemptKey, ToolAttemptRecord>>,
    max_attempts: usize,
}

impl ToolRuntimeAuthority {
    pub(crate) fn new(max_attempts: usize) -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
            max_attempts,
        }
    }

    /// Atomically claims first evaluation of one canonical call attempt.
    pub(crate) fn begin(&self, key: ToolAttemptKey, binding: Digest) -> ToolAttemptClaim {
        self.claim(key, binding, ToolAttemptState::Evaluating)
    }

    /// Atomically restores only the exact pre-dispatch decision state.
    pub(crate) fn restore_pending(&self, key: ToolAttemptKey, binding: Digest) -> ToolAttemptClaim {
        self.claim(key, binding, ToolAttemptState::DecisionPending)
    }

    fn claim(
        &self,
        key: ToolAttemptKey,
        binding: Digest,
        state: ToolAttemptState,
    ) -> ToolAttemptClaim {
        let mut attempts = match self.attempts.lock() {
            Ok(attempts) => attempts,
            Err(poisoned) => poisoned.into_inner(),
        };
        if attempts.contains_key(&key) {
            ToolAttemptClaim::Duplicate
        } else if attempts.len() >= self.max_attempts {
            ToolAttemptClaim::CapacityExceeded
        } else {
            attempts.insert(key, ToolAttemptRecord { binding, state });
            ToolAttemptClaim::Claimed
        }
    }

    /// Applies one exact state transition without reopening a terminal attempt.
    pub(crate) fn transition(
        &self,
        key: ToolAttemptKey,
        binding: Digest,
        expected: ToolAttemptState,
        next: ToolAttemptState,
    ) -> bool {
        let mut attempts = match self.attempts.lock() {
            Ok(attempts) => attempts,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(current) = attempts.get_mut(&key) else {
            return false;
        };
        if current.binding != binding || current.state != expected {
            return false;
        }
        current.state = next;
        true
    }

    /// Releases one exact pre-dispatch state after it can no longer dispatch.
    ///
    /// Dispatched attempts instead transition to a retained terminal tombstone
    /// so one canonical attempt can never be admitted twice in this runtime.
    pub(crate) fn release(
        &self,
        key: ToolAttemptKey,
        binding: Digest,
        expected: ToolAttemptState,
    ) -> bool {
        let mut attempts = match self.attempts.lock() {
            Ok(attempts) => attempts,
            Err(poisoned) => poisoned.into_inner(),
        };
        if attempts
            .get(&key)
            .is_some_and(|current| current.binding == binding && current.state == expected)
        {
            attempts.remove(&key);
            true
        } else {
            false
        }
    }
}

/// An exact invocation admitted for one dispatch by policy or an operator.
///
/// This value is intentionally neither cloneable nor serializable. Executing it
/// consumes the capability, which prevents one grant from being dispatched
/// twice through the safe API.
pub struct AuthorizedToolInvocation {
    invocation: ToolInvocation,
    grant: EffectAuthorizationGrant,
    fence: LiveDispatchFence,
    capability: ToolAttemptCapability,
}

/// Fresh process-local dispatch authority. It has no serializable form.
///
/// The stable authorization binding can survive restart; this fence cannot. A
/// restored allow mints a new fence only after the current runtime, executable
/// registry snapshot, executor bindings, environment allocation, and attempt
/// claim have all been revalidated.
struct LiveDispatchFence {
    runtime: Arc<ToolRuntimeAuthority>,
    registry: FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
}

impl LiveDispatchFence {
    fn new(
        runtime: Arc<ToolRuntimeAuthority>,
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
    ) -> Self {
        Self {
            runtime,
            registry,
            environment,
        }
    }

    fn matches(
        &self,
        runtime: &Arc<ToolRuntimeAuthority>,
        registry: &FrozenToolRegistry,
        environment: &Arc<ExecutionEnvironment>,
    ) -> bool {
        Arc::ptr_eq(&self.runtime, runtime)
            && self.registry.is_same_snapshot(registry)
            && Arc::ptr_eq(&self.environment, environment)
            && environment
                .verify_descriptor(environment.descriptor())
                .is_ok()
    }
}

struct ToolAttemptCapability {
    authority: Arc<ToolRuntimeAuthority>,
    key: ToolAttemptKey,
    binding: Digest,
    state: ToolAttemptState,
    armed: bool,
}

impl ToolAttemptCapability {
    fn new(
        authority: Arc<ToolRuntimeAuthority>,
        invocation: &ToolInvocation,
        binding: Digest,
        state: ToolAttemptState,
    ) -> Self {
        Self {
            authority,
            key: ToolAttemptKey::for_invocation(invocation),
            binding,
            state,
            armed: true,
        }
    }

    fn belongs_to(&self, authority: &Arc<ToolRuntimeAuthority>) -> bool {
        Arc::ptr_eq(&self.authority, authority)
    }

    fn into_authority(mut self) -> Arc<ToolRuntimeAuthority> {
        self.armed = false;
        Arc::clone(&self.authority)
    }

    fn transition(&mut self, next: ToolAttemptState) -> bool {
        if self
            .authority
            .transition(self.key, self.binding, self.state, next)
        {
            self.state = next;
            true
        } else {
            false
        }
    }
}

impl Drop for ToolAttemptCapability {
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

impl AuthorizedToolInvocation {
    pub(crate) fn new(
        invocation: ToolInvocation,
        grant: EffectAuthorizationGrant,
        authority: Arc<ToolRuntimeAuthority>,
        registry: FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
    ) -> Self {
        let fence = LiveDispatchFence::new(Arc::clone(&authority), registry, environment);
        let capability = ToolAttemptCapability::new(
            authority,
            &invocation,
            grant.binding().digest(),
            ToolAttemptState::Authorized,
        );
        Self {
            invocation,
            grant,
            fence,
            capability,
        }
    }

    /// Returns the exact validated invocation covered by the grant.
    #[must_use]
    pub const fn invocation(&self) -> &ToolInvocation {
        &self.invocation
    }

    /// Returns the policy or operator grant covering the invocation.
    #[must_use]
    pub const fn grant(&self) -> &EffectAuthorizationGrant {
        &self.grant
    }

    pub(crate) fn matches_live_fence(
        &self,
        authority: &Arc<ToolRuntimeAuthority>,
        registry: &FrozenToolRegistry,
        environment: &Arc<ExecutionEnvironment>,
    ) -> bool {
        self.capability.belongs_to(authority)
            && self.fence.matches(authority, registry, environment)
    }

    pub(crate) fn mark_journaled(&mut self) -> bool {
        self.capability.transition(ToolAttemptState::Journaled)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ToolInvocation,
        EffectAuthorizationGrant,
        Arc<ToolRuntimeAuthority>,
    ) {
        let Self {
            invocation,
            grant,
            fence: _,
            capability,
        } = self;
        (invocation, grant, capability.into_authority())
    }
}

impl fmt::Debug for AuthorizedToolInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedToolInvocation")
            .field("invocation", &self.invocation)
            .field("grant", &self.grant)
            .field("live_dispatch_fence", &"<bound>")
            .finish_non_exhaustive()
    }
}

/// A policy-allowed request awaiting one explicit control-plane decision.
///
/// The descriptive prompt is not authority. Resolution revalidates the exact
/// digest binding and the process-local runtime capability before minting an
/// [`AuthorizedToolInvocation`].
pub struct PendingEffectDecision {
    invocation: ToolInvocation,
    binding: EffectAuthorizationBinding,
    prompt: EffectDecisionPrompt,
    capability: ToolAttemptCapability,
}

impl PendingEffectDecision {
    pub(crate) fn new(
        invocation: ToolInvocation,
        binding: EffectAuthorizationBinding,
        prompt: EffectDecisionPrompt,
        authority: Arc<ToolRuntimeAuthority>,
    ) -> Self {
        let capability = ToolAttemptCapability::new(
            authority,
            &invocation,
            binding.digest(),
            ToolAttemptState::DecisionPending,
        );
        Self {
            invocation,
            binding,
            prompt,
            capability,
        }
    }

    /// Returns the exact validated invocation awaiting a decision.
    #[must_use]
    pub const fn invocation(&self) -> &ToolInvocation {
        &self.invocation
    }

    /// Returns the authoritative digest binding shown by the control plane.
    #[must_use]
    pub const fn binding(&self) -> &EffectAuthorizationBinding {
        &self.binding
    }

    /// Returns non-authoritative, sanitized operator-facing text.
    #[must_use]
    pub const fn prompt(&self) -> &EffectDecisionPrompt {
        &self.prompt
    }

    pub(crate) fn belongs_to(&self, authority: &Arc<ToolRuntimeAuthority>) -> bool {
        self.capability.belongs_to(authority)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ToolInvocation,
        EffectAuthorizationBinding,
        Arc<ToolRuntimeAuthority>,
    ) {
        let Self {
            invocation,
            binding,
            prompt: _,
            capability,
        } = self;
        (invocation, binding, capability.into_authority())
    }
}

impl fmt::Debug for PendingEffectDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingEffectDecision")
            .field("invocation", &self.invocation)
            .field("binding", &self.binding)
            .field("prompt", &self.prompt)
            .field("capability", &"<bound>")
            .finish()
    }
}

/// Result of evaluating one exact prepared invocation.
#[derive(Debug)]
#[non_exhaustive]
pub enum EffectAuthorization {
    /// Dispatch may proceed once with the enclosed opaque capability.
    Allowed(AuthorizedToolInvocation),
    /// Dispatch is forbidden and no executor was reached.
    Denied(ToolPolicyDenial),
    /// Dispatch is paused for an explicit control-plane decision.
    DecisionRequired(PendingEffectDecision),
}
