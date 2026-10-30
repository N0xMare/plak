//! Internal classified request gate for explicitly admitted subscription work.
//!
//! This module is a cross-crate contract for native provider implementations.
//! A provider must acquire and authorize a permit immediately before each
//! physical transport dispatch. Merely wrapping a `crate::LanguageModel`
//! would not cover authentication refresh or catalog traffic and is therefore
//! not an implementation of this contract.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Physical native-subscription request classes covered by one smoke run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionRequestClass {
    /// Credential refresh or rotation traffic.
    AuthRefresh,
    /// Authenticated model-catalog discovery traffic.
    Catalog,
    /// Model inference traffic, including tool-continuation rounds.
    Inference,
}

/// Immutable classified physical-request and deadline limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionRequestLimits {
    auth_refresh: u32,
    catalog: u32,
    inference: u32,
    max_inference_input_tokens: u64,
    max_elapsed: Option<Duration>,
    retries: u32,
}

impl SubscriptionRequestLimits {
    /// Creates a classified limit set.
    ///
    /// `max_elapsed` is an optional elapsed-time bound on each gate built from
    /// these limits; `None` bounds a gate by its class allowances alone, which
    /// is what a product coding turn uses because a turn has no wall clock. A
    /// nonzero retry allowance is accepted by the generic contract, but the
    /// subscription smoke profile fixes it at zero.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionLimitError::InvalidLimits`] for a zero deadline.
    pub fn new(
        auth_refresh: u32,
        catalog: u32,
        inference: u32,
        max_inference_input_tokens: u64,
        max_elapsed: Option<Duration>,
        retries: u32,
    ) -> Result<Self, SubscriptionLimitError> {
        if max_elapsed.is_some_and(|elapsed| elapsed.is_zero())
            || (inference > 0 && max_inference_input_tokens == 0)
        {
            return Err(SubscriptionLimitError::InvalidLimits);
        }
        Ok(Self {
            auth_refresh,
            catalog,
            inference,
            max_inference_input_tokens,
            max_elapsed,
            retries,
        })
    }

    /// Returns the allowance for one class.
    #[must_use]
    pub const fn allowance(self, class: SubscriptionRequestClass) -> u32 {
        match class {
            SubscriptionRequestClass::AuthRefresh => self.auth_refresh,
            SubscriptionRequestClass::Catalog => self.catalog,
            SubscriptionRequestClass::Inference => self.inference,
        }
    }

    /// Returns the elapsed-time limit, if the gate has one.
    #[must_use]
    pub const fn max_elapsed(self) -> Option<Duration> {
        self.max_elapsed
    }

    /// Returns the aggregate conservative input-token upper bound for inference.
    #[must_use]
    pub const fn max_inference_input_tokens(self) -> u64 {
        self.max_inference_input_tokens
    }

    /// Returns the physical retry allowance.
    #[must_use]
    pub const fn retries(self) -> u32 {
        self.retries
    }
}

/// Monotonic time source used by a classified request gate.
pub trait SubscriptionGateClock: Send + Sync + 'static {
    /// Returns monotonic elapsed time in an arbitrary fixed epoch.
    fn now(&self) -> Duration;
}

/// Process-monotonic production clock.
#[derive(Debug)]
pub struct SystemSubscriptionGateClock {
    origin: Instant,
}

impl Default for SystemSubscriptionGateClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl SubscriptionGateClock for SystemSubscriptionGateClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// One class's deterministic gate accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionClassSnapshot {
    allowance: u32,
    reserved: u32,
    dispatched: u32,
    abandoned: u32,
    denied: u32,
    input_token_upper_bound: u64,
}

impl SubscriptionClassSnapshot {
    /// Returns the immutable allowance.
    #[must_use]
    pub const fn allowance(self) -> u32 {
        self.allowance
    }

    /// Returns reservations still charged, including dispatched and abandoned ones.
    #[must_use]
    pub const fn reserved(self) -> u32 {
        self.reserved
    }

    /// Returns authorized dispatches, excluding proven `NotStarted` refunds.
    #[must_use]
    pub const fn dispatched(self) -> u32 {
        self.dispatched
    }

    /// Returns reserved permits dropped without dispatch authorization.
    #[must_use]
    pub const fn abandoned(self) -> u32 {
        self.abandoned
    }

    /// Returns denied acquisition attempts.
    #[must_use]
    pub const fn denied(self) -> u32 {
        self.denied
    }

    /// Returns the sum of conservative input-token upper bounds admitted for this class.
    ///
    /// This is nonzero only for inference dispatches.
    #[must_use]
    pub const fn input_token_upper_bound(self) -> u64 {
        self.input_token_upper_bound
    }
}

/// Content-free state of one request gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionLimitSnapshot {
    auth_refresh: SubscriptionClassSnapshot,
    catalog: SubscriptionClassSnapshot,
    inference: SubscriptionClassSnapshot,
    elapsed: Duration,
    max_elapsed: Option<Duration>,
    deadline_reached: bool,
    retries: u32,
}

impl SubscriptionLimitSnapshot {
    /// Returns accounting for one request class.
    #[must_use]
    pub const fn class(self, class: SubscriptionRequestClass) -> SubscriptionClassSnapshot {
        match class {
            SubscriptionRequestClass::AuthRefresh => self.auth_refresh,
            SubscriptionRequestClass::Catalog => self.catalog,
            SubscriptionRequestClass::Inference => self.inference,
        }
    }

    /// Returns elapsed monotonic time since gate construction.
    #[must_use]
    pub const fn elapsed(self) -> Duration {
        self.elapsed
    }

    /// Returns the immutable elapsed-time limit, if the gate has one.
    #[must_use]
    pub const fn max_elapsed(self) -> Option<Duration> {
        self.max_elapsed
    }

    /// Reports whether the absolute deadline has been reached.
    #[must_use]
    pub const fn deadline_reached(self) -> bool {
        self.deadline_reached
    }

    /// Returns the immutable retry allowance.
    #[must_use]
    pub const fn retries(self) -> u32 {
        self.retries
    }
}

/// Redaction-safe classified gate failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionLimitError {
    /// A limit set is structurally invalid.
    InvalidLimits,
    /// The one absolute deadline has been reached.
    DeadlineExceeded,
    /// The selected request class has no remaining allowance.
    ClassLimitExceeded {
        /// Class whose reservation counter is exhausted.
        class: SubscriptionRequestClass,
        /// Reservations already charged, including abandoned permits.
        reserved: u32,
        /// Immutable class allowance.
        allowance: u32,
    },
    /// A permit was authorized more than once.
    PermitAlreadyUsed,
    /// Inference dispatch omitted or supplied an invalid conservative input bound.
    InferenceBoundRequired,
    /// The aggregate conservative inference-input bound would be exceeded.
    InferenceInputLimitExceeded,
    /// The exact inference-root identity is unclaimed, already live, or retired.
    InferenceScopeUnavailable,
    /// Gate accounting could not be safely updated.
    Unavailable,
}

impl fmt::Display for SubscriptionLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("subscription request limits are invalid"),
            Self::DeadlineExceeded => {
                formatter.write_str("subscription request deadline was reached")
            }
            Self::ClassLimitExceeded {
                class,
                reserved,
                allowance,
            } => write!(
                formatter,
                "subscription {class:?} reserved {reserved} of allowance {allowance}"
            ),
            Self::PermitAlreadyUsed => {
                formatter.write_str("subscription request permit was already used")
            }
            Self::InferenceBoundRequired => formatter
                .write_str("subscription inference dispatch requires a conservative input bound"),
            Self::InferenceInputLimitExceeded => {
                formatter.write_str("subscription inference input limit was reached")
            }
            Self::InferenceScopeUnavailable => {
                formatter.write_str("subscription inference root scope is unavailable")
            }
            Self::Unavailable => formatter.write_str("subscription request gate is unavailable"),
        }
    }
}

impl std::error::Error for SubscriptionLimitError {}

/// A non-cloneable reservation for exactly one classified dispatch.
pub trait SubscriptionRequestPermit: Send + 'static {
    /// Returns the reserved class.
    fn class(&self) -> SubscriptionRequestClass;

    /// Authorizes the caller's immediately following transport dispatch once.
    ///
    /// This must be called at most once and immediately before transport
    /// execution. Dropping a permit before this call records an abandonment.
    ///
    /// # Errors
    ///
    /// Fails closed after the deadline or after prior authorization.
    fn authorize_dispatch(&mut self) -> Result<(), SubscriptionLimitError>;

    /// Refunds an authorized request only after trusted transport proof that
    /// transmission was `NotStarted`. Incomplete or ambiguous sends never qualify.
    /// The authorization remains spent; a retry must acquire a fresh permit.
    ///
    /// # Errors
    ///
    /// Rejects an unauthorized or already refunded permit and unavailable accounting.
    fn refund_not_started(&mut self) -> Result<(), SubscriptionLimitError>;
}

/// Object-safe classified request gate shared by all native paths in one run.
pub trait SubscriptionRequestGate: Send + Sync + 'static {
    /// Reserves one request from the selected class.
    ///
    /// Reservation consumes class capacity even when the permit is later
    /// abandoned, preventing cancellation or retry from minting new capacity.
    ///
    /// # Errors
    ///
    /// Fails closed at the class limit, absolute deadline, or poisoned state.
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError>;

    /// Reserves one inference dispatch and its conservative input-token upper bound.
    ///
    /// Callers must derive the nonzero bound from the fully lowered,
    /// credential-free request and acquire this permit before materializing a
    /// credential. Dropping the permit consumes both count and bound capacity.
    ///
    /// # Errors
    ///
    /// Fails at the inference count limit, aggregate input limit, absolute
    /// deadline, zero bound, arithmetic overflow, or poisoned state.
    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError>;

    /// Returns a content-free deterministic snapshot.
    ///
    /// # Errors
    ///
    /// Fails closed when state cannot be safely read.
    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError>;
}

/// Opaque exact identity of one admitted inference root's budget.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionInferenceScope {
    /// Ordinary root turn UUID bytes.
    Turn([u8; 16]),
    /// Session-workflow root bytes used by compaction and similar operations.
    SessionWorkflow([u8; 32]),
}

impl SubscriptionInferenceScope {
    /// Creates a nonzero turn scope from its exact opaque UUID bytes.
    ///
    /// # Errors
    ///
    /// Rejects the all-zero identity.
    pub fn turn(bytes: [u8; 16]) -> Result<Self, SubscriptionLimitError> {
        if bytes == [0; 16] {
            Err(SubscriptionLimitError::InvalidLimits)
        } else {
            Ok(Self::Turn(bytes))
        }
    }

    /// Creates a nonzero session-workflow scope from its exact opaque bytes.
    pub fn session_workflow(bytes: [u8; 32]) -> Result<Self, SubscriptionLimitError> {
        if bytes == [0; 32] {
            Err(SubscriptionLimitError::InvalidLimits)
        } else {
            Ok(Self::SessionWorkflow(bytes))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ClassState {
    dispatch_attempts: u32,
    reserved: u32,
    dispatched: u32,
    abandoned: u32,
    denied: u32,
    input_token_upper_bound: u64,
}

#[derive(Debug, Default)]
struct GateState {
    auth_refresh: ClassState,
    catalog: ClassState,
    inference: ClassState,
}

impl GateState {
    fn class_mut(&mut self, class: SubscriptionRequestClass) -> &mut ClassState {
        match class {
            SubscriptionRequestClass::AuthRefresh => &mut self.auth_refresh,
            SubscriptionRequestClass::Catalog => &mut self.catalog,
            SubscriptionRequestClass::Inference => &mut self.inference,
        }
    }

    const fn class(&self, class: SubscriptionRequestClass) -> ClassState {
        match class {
            SubscriptionRequestClass::AuthRefresh => self.auth_refresh,
            SubscriptionRequestClass::Catalog => self.catalog,
            SubscriptionRequestClass::Inference => self.inference,
        }
    }
}

struct GateInner {
    limits: SubscriptionRequestLimits,
    clock: Arc<dyn SubscriptionGateClock>,
    started: Duration,
    state: Mutex<GateState>,
}

impl fmt::Debug for GateInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionRequestGate")
            .field("limits", &self.limits)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl GateInner {
    fn elapsed(&self) -> Duration {
        self.clock.now().saturating_sub(self.started)
    }

    fn deadline_reached(&self) -> bool {
        self.limits
            .max_elapsed
            .is_some_and(|limit| self.elapsed() >= limit)
    }
}

/// In-process reference gate for provider integration and deterministic tests.
#[derive(Clone)]
pub struct InMemorySubscriptionRequestGate {
    inner: Arc<GateInner>,
}

impl fmt::Debug for InMemorySubscriptionRequestGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl InMemorySubscriptionRequestGate {
    /// Creates a gate whose deadline begins immediately.
    #[must_use]
    pub fn new(limits: SubscriptionRequestLimits, clock: Arc<dyn SubscriptionGateClock>) -> Self {
        let started = clock.now();
        Self {
            inner: Arc::new(GateInner {
                limits,
                clock,
                started,
                state: Mutex::new(GateState::default()),
            }),
        }
    }
}

thread_local! {
    static ACTIVE_INFERENCE_SCOPES: RefCell<Vec<(u64, InMemorySubscriptionRequestGate)>> =
        const { RefCell::new(Vec::new()) };
}

static NEXT_TURN_GATE_ID: AtomicU64 = AtomicU64::new(1);

/// Immutable capability for one exact inference root's accounting state.
#[derive(Clone)]
pub struct SubscriptionInferenceScopeHandle {
    gate_id: u64,
    scope: SubscriptionInferenceScope,
    inference: InMemorySubscriptionRequestGate,
    global: InMemorySubscriptionRequestGate,
}

/// Sole lifetime capability for one admitted inference root.
///
/// Dropping the lease closes the root and permanently retires its identity for
/// this gate. Prepared calls borrow accounting through handles, but those
/// handles never mint or renew a root budget.
pub struct SubscriptionInferenceScopeLease {
    gate: Arc<TurnScopedSubscriptionRequestGate>,
    handle: SubscriptionInferenceScopeHandle,
}

impl fmt::Debug for SubscriptionInferenceScopeLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionInferenceScopeLease")
            .field("scope", &self.handle.scope)
            .finish_non_exhaustive()
    }
}

impl SubscriptionInferenceScopeLease {
    /// Returns a poll-local accounting handle without transferring lifetime
    /// authority.
    #[must_use]
    pub fn handle(&self) -> SubscriptionInferenceScopeHandle {
        self.handle.clone()
    }

    /// Returns the exact root identity owned by this lease.
    #[must_use]
    pub const fn scope(&self) -> SubscriptionInferenceScope {
        self.handle.scope
    }
}

impl Drop for SubscriptionInferenceScopeLease {
    fn drop(&mut self) {
        let mut scopes = match self.gate.scopes.lock() {
            Ok(scopes) => scopes,
            Err(poisoned) => poisoned.into_inner(),
        };
        scopes.live.remove(&self.handle.scope);
        scopes.retired.insert(self.handle.scope);
    }
}

impl fmt::Debug for SubscriptionInferenceScopeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionInferenceScopeHandle")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SubscriptionInferenceScopeHandle {
    /// Returns the exact root identity bound to this capability.
    #[must_use]
    pub const fn scope(&self) -> SubscriptionInferenceScope {
        self.scope
    }

    /// Enters this scope for synchronous construction or one future poll.
    ///
    /// The returned guard must remain alive only while the provider-owned
    /// prepared call is being constructed or polled.
    #[must_use]
    pub fn enter(&self) -> SubscriptionInferenceScopeGuard {
        ACTIVE_INFERENCE_SCOPES.with(|active| {
            active
                .borrow_mut()
                .push((self.gate_id, self.inference.clone()));
        });
        SubscriptionInferenceScopeGuard {
            gate_id: self.gate_id,
        }
    }

    /// Returns a snapshot whose inference facts and deadline belong to this
    /// exact scope while auth/catalog facts remain qualification-global.
    pub fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        scope_snapshot(&self.global, &self.inference)
    }
}

/// Poll-local activation guard for a scope-bound inference capability.
pub struct SubscriptionInferenceScopeGuard {
    gate_id: u64,
}

impl Drop for SubscriptionInferenceScopeGuard {
    fn drop(&mut self) {
        ACTIVE_INFERENCE_SCOPES.with(|active| {
            let popped = active.borrow_mut().pop();
            debug_assert!(popped.is_some_and(|(gate_id, _)| gate_id == self.gate_id));
        });
    }
}

/// Gate whose global auth/catalog authority is shared while inference
/// authority is captured as an immutable root-scoped preparation capability.
pub struct TurnScopedSubscriptionRequestGate {
    limits: SubscriptionRequestLimits,
    clock: Arc<dyn SubscriptionGateClock>,
    global: InMemorySubscriptionRequestGate,
    gate_id: u64,
    scopes: Mutex<InferenceScopes>,
}

#[derive(Default)]
struct InferenceScopes {
    live: HashMap<SubscriptionInferenceScope, InMemorySubscriptionRequestGate>,
    retired: HashSet<SubscriptionInferenceScope>,
}

impl fmt::Debug for TurnScopedSubscriptionRequestGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnScopedSubscriptionRequestGate")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl TurnScopedSubscriptionRequestGate {
    /// Creates a gate with no active inference turn.
    #[must_use]
    pub fn new(limits: SubscriptionRequestLimits, clock: Arc<dyn SubscriptionGateClock>) -> Self {
        Self {
            limits,
            clock: Arc::clone(&clock),
            global: InMemorySubscriptionRequestGate::new(limits, clock),
            gate_id: NEXT_TURN_GATE_ID.fetch_add(1, Ordering::Relaxed),
            scopes: Mutex::new(InferenceScopes::default()),
        }
    }

    /// Claims the sole nonrenewable lifetime capability for one exact root.
    ///
    /// # Errors
    ///
    /// Fails closed if the identity is already live, was previously retired,
    /// or gate state is unavailable.
    pub fn claim_scope(
        self: &Arc<Self>,
        scope: SubscriptionInferenceScope,
    ) -> Result<SubscriptionInferenceScopeLease, SubscriptionLimitError> {
        let mut scopes = self
            .scopes
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        if scopes.live.contains_key(&scope) || scopes.retired.contains(&scope) {
            return Err(SubscriptionLimitError::InferenceScopeUnavailable);
        }
        let inference = InMemorySubscriptionRequestGate::new(self.limits, Arc::clone(&self.clock));
        scopes.live.insert(scope, inference.clone());
        let handle = SubscriptionInferenceScopeHandle {
            gate_id: self.gate_id,
            scope,
            inference,
            global: self.global.clone(),
        };
        Ok(SubscriptionInferenceScopeLease {
            gate: Arc::clone(self),
            handle,
        })
    }

    /// Borrows the accounting handle of an already-claimed live root.
    ///
    /// This operation cannot mint authority and therefore rejects unknown and
    /// retired identities.
    pub fn bind_scope(
        &self,
        scope: SubscriptionInferenceScope,
    ) -> Result<SubscriptionInferenceScopeHandle, SubscriptionLimitError> {
        let scopes = self
            .scopes
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let inference = scopes
            .live
            .get(&scope)
            .cloned()
            .ok_or(SubscriptionLimitError::InferenceScopeUnavailable)?;
        Ok(SubscriptionInferenceScopeHandle {
            gate_id: self.gate_id,
            scope,
            inference,
            global: self.global.clone(),
        })
    }

    fn active_inference_gate(&self) -> Option<InMemorySubscriptionRequestGate> {
        if let Some(gate) = ACTIVE_INFERENCE_SCOPES.with(|active| {
            active
                .borrow()
                .iter()
                .rev()
                .find(|(gate_id, _)| *gate_id == self.gate_id)
                .map(|(_, gate)| gate.clone())
        }) {
            return Some(gate);
        }
        // A single claimed live root can acquire without TLS. Zero roots or
        // two live roots (turn + compact workflow) must not pick HashMap order.
        let scopes = self.scopes.lock().ok()?;
        if scopes.live.len() != 1 {
            return None;
        }
        scopes.live.values().next().cloned()
    }
}

impl SubscriptionRequestGate for TurnScopedSubscriptionRequestGate {
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        if class == SubscriptionRequestClass::Inference {
            return Err(SubscriptionLimitError::InferenceBoundRequired);
        }
        // A refresh inside a turn is that turn's traffic. Charging it to the
        // root keeps an attached session's auth and catalog refreshes from
        // spending one lifetime allowance; with no determinable root (at
        // resolution, or two live roots) the shared gate pays.
        self.active_inference_gate()
            .unwrap_or_else(|| self.global.clone())
            .acquire(class)
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        self.active_inference_gate()
            .ok_or(SubscriptionLimitError::InferenceScopeUnavailable)?
            .acquire_inference(input_token_upper_bound)
    }

    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        let global = self.global.snapshot()?;
        let inference = self
            .active_inference_gate()
            .map(|gate| gate.snapshot())
            .transpose()?
            .map_or_else(
                || SubscriptionClassSnapshot {
                    allowance: self.limits.allowance(SubscriptionRequestClass::Inference),
                    reserved: 0,
                    dispatched: 0,
                    abandoned: 0,
                    denied: 0,
                    input_token_upper_bound: 0,
                },
                |snapshot| snapshot.class(SubscriptionRequestClass::Inference),
            );
        Ok(SubscriptionLimitSnapshot {
            auth_refresh: global.class(SubscriptionRequestClass::AuthRefresh),
            catalog: global.class(SubscriptionRequestClass::Catalog),
            inference,
            elapsed: global.elapsed(),
            max_elapsed: global.max_elapsed(),
            deadline_reached: global.deadline_reached(),
            retries: global.retries(),
        })
    }
}

fn scope_snapshot(
    global_gate: &InMemorySubscriptionRequestGate,
    inference_gate: &InMemorySubscriptionRequestGate,
) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
    let global = global_gate.snapshot()?;
    let inference = inference_gate.snapshot()?;
    Ok(SubscriptionLimitSnapshot {
        auth_refresh: global.class(SubscriptionRequestClass::AuthRefresh),
        catalog: global.class(SubscriptionRequestClass::Catalog),
        inference: inference.class(SubscriptionRequestClass::Inference),
        elapsed: inference.elapsed(),
        max_elapsed: inference.max_elapsed(),
        deadline_reached: inference.deadline_reached(),
        retries: inference.retries(),
    })
}

impl InMemorySubscriptionRequestGate {
    /// Returns all dispatch authorizations, including proven `NotStarted` refunds.
    /// This is process-local attempt evidence, not charged request capacity.
    ///
    /// # Errors
    /// Returns unavailable accounting if the gate's lock is poisoned.
    pub fn dispatch_attempts(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<u32, SubscriptionLimitError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        Ok(state.class(class).dispatch_attempts)
    }
}

impl SubscriptionRequestGate for InMemorySubscriptionRequestGate {
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        if class == SubscriptionRequestClass::Inference {
            return Err(SubscriptionLimitError::InferenceBoundRequired);
        }
        self.acquire_inner(class, None)
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        if input_token_upper_bound == 0 {
            return Err(SubscriptionLimitError::InferenceBoundRequired);
        }
        self.acquire_inner(
            SubscriptionRequestClass::Inference,
            Some(input_token_upper_bound),
        )
    }

    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let snapshot = |class| {
            let value = state.class(class);
            SubscriptionClassSnapshot {
                allowance: self.inner.limits.allowance(class),
                reserved: value.reserved,
                dispatched: value.dispatched,
                abandoned: value.abandoned,
                denied: value.denied,
                input_token_upper_bound: value.input_token_upper_bound,
            }
        };
        Ok(SubscriptionLimitSnapshot {
            auth_refresh: snapshot(SubscriptionRequestClass::AuthRefresh),
            catalog: snapshot(SubscriptionRequestClass::Catalog),
            inference: snapshot(SubscriptionRequestClass::Inference),
            elapsed: self.inner.elapsed(),
            max_elapsed: self.inner.limits.max_elapsed,
            deadline_reached: self.inner.deadline_reached(),
            retries: self.inner.limits.retries,
        })
    }
}

impl InMemorySubscriptionRequestGate {
    fn acquire_inner(
        &self,
        class: SubscriptionRequestClass,
        input_token_upper_bound: Option<u64>,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let class_state = state.class_mut(class);
        if self.inner.deadline_reached() {
            class_state.denied = class_state
                .denied
                .checked_add(1)
                .ok_or(SubscriptionLimitError::Unavailable)?;
            return Err(SubscriptionLimitError::DeadlineExceeded);
        }
        let allowance = self.inner.limits.allowance(class);
        if class_state.reserved >= allowance {
            class_state.denied = class_state
                .denied
                .checked_add(1)
                .ok_or(SubscriptionLimitError::Unavailable)?;
            return Err(SubscriptionLimitError::ClassLimitExceeded {
                class,
                reserved: class_state.reserved,
                allowance,
            });
        }
        if let Some(bound) = input_token_upper_bound {
            let candidate = class_state
                .input_token_upper_bound
                .checked_add(bound)
                .ok_or(SubscriptionLimitError::Unavailable)?;
            if candidate > self.inner.limits.max_inference_input_tokens {
                class_state.denied = class_state
                    .denied
                    .checked_add(1)
                    .ok_or(SubscriptionLimitError::Unavailable)?;
                return Err(SubscriptionLimitError::InferenceInputLimitExceeded);
            }
            class_state.input_token_upper_bound = candidate;
        }
        class_state.reserved = class_state
            .reserved
            .checked_add(1)
            .ok_or(SubscriptionLimitError::Unavailable)?;
        Ok(Box::new(InMemoryPermit {
            inner: Arc::clone(&self.inner),
            class,
            authorized: false,
            settled: false,
            refunded: false,
            input_token_upper_bound: input_token_upper_bound.unwrap_or(0),
        }))
    }
}

struct InMemoryPermit {
    inner: Arc<GateInner>,
    class: SubscriptionRequestClass,
    authorized: bool,
    settled: bool,
    refunded: bool,
    input_token_upper_bound: u64,
}

impl SubscriptionRequestPermit for InMemoryPermit {
    fn class(&self) -> SubscriptionRequestClass {
        self.class
    }

    fn authorize_dispatch(&mut self) -> Result<(), SubscriptionLimitError> {
        if self.authorized {
            return Err(SubscriptionLimitError::PermitAlreadyUsed);
        }
        if self.inner.deadline_reached() {
            return Err(SubscriptionLimitError::DeadlineExceeded);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let class = state.class_mut(self.class);
        class.dispatch_attempts = class
            .dispatch_attempts
            .checked_add(1)
            .ok_or(SubscriptionLimitError::Unavailable)?;
        class.dispatched = class
            .dispatched
            .checked_add(1)
            .ok_or(SubscriptionLimitError::Unavailable)?;
        self.authorized = true;
        self.settled = true;
        Ok(())
    }

    fn refund_not_started(&mut self) -> Result<(), SubscriptionLimitError> {
        if !self.authorized || self.refunded {
            return Err(SubscriptionLimitError::PermitAlreadyUsed);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let class = state.class_mut(self.class);
        class.reserved -= 1;
        class.dispatched -= 1;
        class.input_token_upper_bound -= self.input_token_upper_bound;
        self.refunded = true;
        Ok(())
    }
}

impl Drop for InMemoryPermit {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            let class = state.class_mut(self.class);
            class.abandoned = class.abandoned.saturating_add(1);
            self.settled = true;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "gate test fixtures use explicit panic messages for invariant setup"
)]
mod tests;
