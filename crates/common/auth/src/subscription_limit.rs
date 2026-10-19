//! Internal classified request gate for explicitly admitted subscription work.
//!
//! This module is a cross-crate contract for native provider implementations.
//! A provider must acquire and authorize a permit immediately before each
//! physical transport dispatch. Merely wrapping a `plakous_model::LanguageModel`
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

/// Per-physical-attempt catalog authorization and trusted unsent refunds.
#[cfg(feature = "http-refresh")]
pub struct CatalogRequestDispatch {
    gate: Option<Arc<dyn SubscriptionRequestGate>>,
    permit: Option<Box<dyn SubscriptionRequestPermit>>,
}

#[cfg(feature = "http-refresh")]
impl CatalogRequestDispatch {
    /// Uses the caller's existing catalog allowance; no gate means unmetered maintenance.
    #[must_use]
    pub fn new(gate: Option<Arc<dyn SubscriptionRequestGate>>) -> Self {
        Self { gate, permit: None }
    }

    /// `None` authorizes the next dispatch; `Some` settles a transport failure.
    /// Only trusted `NotStarted` proof refunds a spent authorization.
    ///
    /// # Errors
    /// Propagates denied acquisition/authorization or unavailable refund accounting.
    pub fn observe(
        &mut self,
        failure: Option<plakous_http::TransmissionFact>,
    ) -> Result<(), SubscriptionLimitError> {
        if let Some(failure) = failure {
            if failure == plakous_http::TransmissionFact::NotStarted
                && let Some(permit) = &mut self.permit
            {
                permit.refund_not_started()?;
            }
        } else {
            self.permit = None;
            if let Some(gate) = &self.gate {
                let mut permit = gate.acquire(SubscriptionRequestClass::Catalog)?;
                permit.authorize_dispatch()?;
                self.permit = Some(permit);
            }
        }
        Ok(())
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
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    use super::*;

    #[derive(Default)]
    struct ManualClock(AtomicU64);

    impl ManualClock {
        fn advance(&self, seconds: u64) {
            self.0.fetch_add(seconds, Ordering::SeqCst);
        }
    }

    impl SubscriptionGateClock for ManualClock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }

    fn gate(clock: Arc<ManualClock>) -> InMemorySubscriptionRequestGate {
        InMemorySubscriptionRequestGate::new(
            SubscriptionRequestLimits::new(0, 1, 5, 4_096, Some(Duration::from_secs(300)), 0)
                .expect("fixed limits are valid"),
            clock,
        )
    }

    #[test]
    fn proven_not_started_refunds_count_and_input_without_reusing_authorization() {
        let gate = gate(Arc::new(ManualClock::default()));
        let before = gate.snapshot().expect("snapshot").inference;
        let mut permit = gate
            .acquire_inference(4_096)
            .expect("whole input allowance");
        permit.authorize_dispatch().expect("dispatch");
        permit.refund_not_started().expect("proven unsent refund");
        assert_eq!(gate.snapshot().expect("snapshot").inference, before);
        assert_eq!(
            permit.authorize_dispatch(),
            Err(SubscriptionLimitError::PermitAlreadyUsed)
        );
        assert_eq!(
            permit.refund_not_started(),
            Err(SubscriptionLimitError::PermitAlreadyUsed)
        );
        drop(permit);
        assert_eq!(gate.snapshot().expect("snapshot").inference, before);
        assert!(gate.acquire_inference(4_096).is_ok());
    }

    #[test]
    fn zero_refresh_one_catalog_and_five_inference_are_exact() {
        let gate = gate(Arc::new(ManualClock::default()));
        assert_eq!(
            gate.acquire(SubscriptionRequestClass::AuthRefresh).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::AuthRefresh,
                reserved: 0,
                allowance: 0,
            })
        );
        let mut catalog = gate
            .acquire(SubscriptionRequestClass::Catalog)
            .expect("catalog permit");
        catalog.authorize_dispatch().expect("catalog dispatch");
        assert_eq!(
            catalog.authorize_dispatch(),
            Err(SubscriptionLimitError::PermitAlreadyUsed)
        );
        assert!(gate.acquire(SubscriptionRequestClass::Catalog).is_err());
        for _ in 0..5 {
            gate.acquire_inference(100)
                .expect("inference permit")
                .authorize_dispatch()
                .expect("inference dispatch");
        }
        assert_eq!(
            gate.acquire(SubscriptionRequestClass::Inference).err(),
            Some(SubscriptionLimitError::InferenceBoundRequired)
        );
        assert_eq!(
            gate.acquire_inference(100).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 5,
                allowance: 5,
            })
        );

        let snapshot = gate.snapshot().expect("snapshot");
        assert_eq!(snapshot.retries(), 0);
        assert_eq!(
            snapshot.class(SubscriptionRequestClass::AuthRefresh),
            SubscriptionClassSnapshot {
                allowance: 0,
                reserved: 0,
                dispatched: 0,
                abandoned: 0,
                denied: 1,
                input_token_upper_bound: 0,
            }
        );
        assert_eq!(
            snapshot
                .class(SubscriptionRequestClass::Inference)
                .dispatched(),
            5
        );
        assert_eq!(
            snapshot
                .class(SubscriptionRequestClass::Inference)
                .input_token_upper_bound(),
            500
        );
    }

    #[test]
    fn abandoned_permit_consumes_capacity_and_cannot_mint_retry() {
        let gate = gate(Arc::new(ManualClock::default()));
        drop(
            gate.acquire(SubscriptionRequestClass::Catalog)
                .expect("catalog permit"),
        );
        assert_eq!(
            gate.acquire(SubscriptionRequestClass::Catalog).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Catalog,
                reserved: 1,
                allowance: 1,
            })
        );
        let state = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Catalog);
        assert_eq!(state.reserved(), 1);
        assert_eq!(state.dispatched(), 0);
        assert_eq!(state.abandoned(), 1);
        assert_eq!(state.denied(), 1);
    }

    #[test]
    fn one_absolute_deadline_applies_to_existing_and_new_permits() {
        let clock = Arc::new(ManualClock::default());
        let gate = gate(clock.clone());
        let mut permit = gate.acquire_inference(1).expect("pre-deadline permit");
        clock.advance(300);
        assert_eq!(
            permit.authorize_dispatch(),
            Err(SubscriptionLimitError::DeadlineExceeded)
        );
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::DeadlineExceeded)
        );
        drop(permit);
        let snapshot = gate.snapshot().expect("snapshot");
        assert!(snapshot.deadline_reached());
        assert_eq!(snapshot.elapsed(), Duration::from_secs(300));
        assert_eq!(snapshot.max_elapsed(), Some(Duration::from_secs(300)));
    }

    #[test]
    fn concurrent_reservations_never_overshoot() {
        let gate = Arc::new(gate(Arc::new(ManualClock::default())));
        let threads = (0..32)
            .map(|_| {
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.acquire_inference(1)
                        .map(|mut permit| permit.authorize_dispatch())
                })
            })
            .collect::<Vec<_>>();
        let successes = threads
            .into_iter()
            .map(thread::JoinHandle::join)
            .filter(|result| matches!(result, Ok(Ok(Ok(())))))
            .count();
        assert_eq!(successes, 5);
        let state = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(state.reserved(), 5);
        assert_eq!(state.dispatched(), 5);
        assert_eq!(state.denied(), 27);
    }

    #[test]
    fn scope_handles_prevent_a_b_a_renewal_and_keep_truthful_deadlines() {
        let clock = Arc::new(ManualClock::default());
        let limits = SubscriptionRequestLimits::new(0, 1, 2, 10, Some(Duration::from_secs(5)), 0)
            .expect("limits");
        let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            clock.clone(),
        ));
        let first = SubscriptionInferenceScope::turn([1; 16]).expect("first scope");
        let second = SubscriptionInferenceScope::turn([2; 16]).expect("second scope");

        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::InferenceScopeUnavailable),
            "inference before a claimed root must fail closed"
        );
        let first_lease = gate.claim_scope(first).expect("claim first");
        gate.acquire_inference(1)
            .expect("the sole claimed root is unambiguous without a poll-local enter")
            .authorize_dispatch()
            .expect("sole-root dispatch");
        let first_handle = first_lease.handle();
        let first_active = first_handle.enter();
        gate.acquire_inference(5)
            .expect("count permit")
            .authorize_dispatch()
            .expect("count dispatch");
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 2,
                allowance: 2,
            })
        );
        drop(first_active);
        let second_lease = gate.claim_scope(second).expect("claim second");
        let second_handle = second_lease.handle();
        let second_active = second_handle.enter();
        gate.acquire_inference(1)
            .expect("second scope is independent")
            .authorize_dispatch()
            .expect("second dispatch");
        drop(second_active);

        let first_again = gate.bind_scope(first).expect("rebind live first");
        assert_eq!(first_again.scope(), first);
        let first_again_active = first_again.enter();
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 2,
                allowance: 2,
            })
        );
        drop(first_again_active);

        let _second_active = second_handle.enter();
        assert_eq!(
            gate.acquire_inference(11).err(),
            Some(SubscriptionLimitError::InferenceInputLimitExceeded)
        );
        let mut before_deadline = gate.acquire_inference(1).expect("deadline permit");
        clock.advance(5);
        assert_eq!(
            before_deadline.authorize_dispatch(),
            Err(SubscriptionLimitError::DeadlineExceeded)
        );
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::DeadlineExceeded)
        );

        let snapshot = second_handle.snapshot().expect("scope snapshot");
        assert!(snapshot.deadline_reached());
        assert_eq!(snapshot.elapsed(), Duration::from_secs(5));
        let state = snapshot.class(SubscriptionRequestClass::Inference);
        assert_eq!(state.reserved(), 2);
        assert_eq!(state.dispatched(), 1);
        assert_eq!(state.denied(), 2);
        assert_eq!(state.input_token_upper_bound(), 2);

        drop(first_lease);
        assert_eq!(
            gate.claim_scope(first).err(),
            Some(SubscriptionLimitError::InferenceScopeUnavailable)
        );
        drop(second_lease);
    }

    #[test]
    fn concurrent_scope_activation_never_cross_charges() {
        let clock = Arc::new(ManualClock::default());
        let limits = SubscriptionRequestLimits::new(0, 1, 1, 10, Some(Duration::from_secs(5)), 0)
            .expect("limits");
        let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
        let first_lease = gate
            .claim_scope(SubscriptionInferenceScope::turn([1; 16]).expect("first"))
            .expect("claim first");
        let second_lease = gate
            .claim_scope(SubscriptionInferenceScope::turn([2; 16]).expect("second"))
            .expect("claim second");
        let first = first_lease.handle();
        let second = second_lease.handle();
        let threads = [first.clone(), second.clone()].map(|scope| {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let _active = scope.enter();
                gate.acquire_inference(3)?.authorize_dispatch()
            })
        });
        for thread in threads {
            assert_eq!(thread.join().expect("thread"), Ok(()));
        }
        for scope in [first, second] {
            let inference = scope
                .snapshot()
                .expect("scope snapshot")
                .class(SubscriptionRequestClass::Inference);
            assert_eq!(inference.reserved(), 1);
            assert_eq!(inference.dispatched(), 1);
            assert_eq!(inference.input_token_upper_bound(), 3);
        }
    }

    #[test]
    fn dyn_gate_acquires_after_typed_claim_without_tls_and_after_lease_drop() {
        let clock = Arc::new(ManualClock::default());
        let limits =
            SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
                .expect("limits");
        let typed = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
        let dyn_gate: Arc<dyn SubscriptionRequestGate> = typed.clone();
        let scope = SubscriptionInferenceScope::turn([9; 16]).expect("scope");
        let lease = typed.claim_scope(scope).expect("claim");
        dyn_gate
            .acquire_inference(100_000)
            .expect("manager-style dyn acquire must succeed on the claimed root without TLS")
            .authorize_dispatch()
            .expect("dispatch");
        drop(lease);
        assert_eq!(
            dyn_gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::InferenceScopeUnavailable)
        );
    }

    #[test]
    fn claimed_root_inference_is_independent_of_a_saturated_process_class() {
        let clock = Arc::new(ManualClock::default());
        let limits =
            SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
                .expect("limits");
        let typed = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
        let dyn_gate: Arc<dyn SubscriptionRequestGate> = typed.clone();
        let global_permits = (0..8)
            .map(|_| {
                typed
                    .global
                    .acquire_inference(1)
                    .expect("process class reservation")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            typed.global.acquire_inference(1).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 8,
                allowance: 8,
            })
        );
        let lease = typed
            .claim_scope(SubscriptionInferenceScope::turn([3; 16]).expect("scope"))
            .expect("claim");
        dyn_gate
            .acquire_inference(1)
            .expect("claimed root must not inherit a saturated process inference class")
            .authorize_dispatch()
            .expect("root dispatch");
        drop(lease);
        drop(global_permits);
    }

    /// A product gate has no elapsed clock, and a turn's refreshes are the
    /// turn's: an attached session spanning hours keeps its auth and catalog
    /// traffic, and a turn longer than any former deadline keeps dispatching.
    #[test]
    fn unclocked_gate_charges_refreshes_to_the_live_root_for_hours() {
        let clock = Arc::new(ManualClock::default());
        let limits = SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, None, 0).expect("limits");
        let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            clock.clone(),
        ));
        let mut resolution = gate
            .acquire(SubscriptionRequestClass::Catalog)
            .expect("resolution catalog");
        resolution
            .authorize_dispatch()
            .expect("resolution dispatch");
        for (turn, seed) in [(1_u64, 1_u8), (2, 2), (3, 3)] {
            clock.advance(turn * 3_600);
            let lease = gate
                .claim_scope(SubscriptionInferenceScope::turn([seed; 16]).expect("scope"))
                .expect("claim");
            for class in [
                SubscriptionRequestClass::AuthRefresh,
                SubscriptionRequestClass::Catalog,
            ] {
                gate.acquire(class)
                    .expect("in-turn refresh is charged to the root")
                    .authorize_dispatch()
                    .expect("in-turn refresh dispatch");
            }
            clock.advance(3_600);
            gate.acquire_inference(1)
                .expect("an hour-long turn keeps inference")
                .authorize_dispatch()
                .expect("inference dispatch");
            assert!(!gate.snapshot().expect("snapshot").deadline_reached());
            drop(lease);
        }
        let global = gate.global.snapshot().expect("global snapshot");
        assert_eq!(global.max_elapsed(), None);
        assert_eq!(
            global.class(SubscriptionRequestClass::Catalog).reserved(),
            1
        );
        assert_eq!(
            global
                .class(SubscriptionRequestClass::AuthRefresh)
                .reserved(),
            0
        );
    }

    #[test]
    fn two_live_roots_do_not_cross_charge_without_tls() {
        let clock = Arc::new(ManualClock::default());
        let limits =
            SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
                .expect("limits");
        let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
        let first = gate
            .claim_scope(SubscriptionInferenceScope::turn([1; 16]).expect("first"))
            .expect("claim first");
        let _second = gate
            .claim_scope(SubscriptionInferenceScope::turn([2; 16]).expect("second"))
            .expect("claim second");
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::InferenceScopeUnavailable)
        );
        let _active = first.handle().enter();
        gate.acquire_inference(1)
            .expect("TLS selects the entered root")
            .authorize_dispatch()
            .expect("dispatch");
    }

    #[test]
    fn eight_abandoned_inference_permits_report_reserved_and_allowance() {
        let limits =
            SubscriptionRequestLimits::new(1, 2, 8, 1_000_000, Some(Duration::from_secs(300)), 0)
                .expect("limits");
        let gate = InMemorySubscriptionRequestGate::new(limits, Arc::new(ManualClock::default()));
        let permits = (0..8)
            .map(|_| gate.acquire_inference(1).expect("inference reservation"))
            .collect::<Vec<_>>();
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 8,
                allowance: 8,
            })
        );
        let state = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(state.reserved(), 8);
        assert_eq!(state.dispatched(), 0);
        assert_eq!(state.abandoned(), 0);
        assert_eq!(state.denied(), 1);
        drop(permits);
        let abandoned = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(abandoned.reserved(), 8);
        assert_eq!(abandoned.abandoned(), 8);
        assert_eq!(
            gate.acquire_inference(1).err(),
            Some(SubscriptionLimitError::ClassLimitExceeded {
                class: SubscriptionRequestClass::Inference,
                reserved: 8,
                allowance: 8,
            })
        );
    }
}

#[cfg(all(test, feature = "http-refresh"))]
#[allow(
    clippy::expect_used,
    reason = "test constants and accounting assertions"
)]
mod catalog_dispatch_tests {
    use super::*;

    #[test]
    fn catalog_attempts_keep_fresh_permits_and_only_refund_not_started() {
        let gate = Arc::new(InMemorySubscriptionRequestGate::new(
            SubscriptionRequestLimits::new(0, 3, 0, 0, None, 0).expect("limits"),
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let mut dispatch = CatalogRequestDispatch::new(Some(gate.clone()));
        dispatch.observe(None).expect("first permit");
        dispatch
            .observe(Some(plakous_http::TransmissionFact::NotStarted))
            .expect("refund");
        dispatch.observe(None).expect("fresh permit");
        dispatch
            .observe(Some(plakous_http::TransmissionFact::MayHaveTransmitted))
            .expect("sent");
        let state = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Catalog);
        assert_eq!(state.reserved(), 1);
        assert_eq!(state.dispatched(), 1);
        assert_eq!(
            gate.dispatch_attempts(SubscriptionRequestClass::Catalog)
                .expect("attempts"),
            2
        );
    }
}
