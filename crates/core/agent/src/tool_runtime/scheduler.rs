//! Atomic, fair, bounded scheduling and timers owned by the agent driver.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use plakous_model::{ToolConcurrency, ToolResourceKey};

use plakous_tools::{ToolRuntimeError, ToolRuntimeErrorKind};

use super::dispatch_error::ToolDispatchError;

const DEFAULT_GLOBAL_CONCURRENCY: usize = 64;
const DEFAULT_RUNTIME_CONCURRENCY: usize = 8;
const DEFAULT_GLOBAL_WAITERS: usize = 4_096;
const DEFAULT_RUNTIME_WAITERS: usize = 1_024;

#[derive(Clone, Debug)]
struct AdmissionRequest {
    runtime_id: u64,
    runtime_capacity: usize,
    serial: bool,
    resources: Vec<ToolResourceKey>,
}

#[derive(Debug)]
struct Waiter {
    ticket: u64,
    request: AdmissionRequest,
    waker: Waker,
}

#[derive(Debug)]
struct Allocation {
    runtime_id: u64,
    serial: bool,
    resources: Vec<ToolResourceKey>,
}

#[derive(Debug, Default)]
struct RuntimeUsage {
    active: usize,
    waiting: usize,
    serial: bool,
}

#[derive(Debug, Default)]
struct SchedulerState {
    next_ticket: u64,
    next_runtime_id: u64,
    active_global: usize,
    runtimes: BTreeMap<u64, RuntimeUsage>,
    active_resources: BTreeSet<ToolResourceKey>,
    waiters: VecDeque<Waiter>,
    granted: BTreeMap<u64, Allocation>,
}

/// Shared process tool scheduler. Clones share one atomic admission domain.
#[derive(Clone, Debug)]
pub struct ToolScheduler {
    inner: Arc<ToolSchedulerInner>,
}

#[derive(Debug)]
struct ToolSchedulerInner {
    state: Mutex<SchedulerState>,
    max_global: usize,
    max_waiters: usize,
}

/// A point-in-time scheduler status snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolSchedulerStatus {
    active: usize,
    waiting: usize,
    active_resource_keys: usize,
    active_runtime_lanes: usize,
}

impl ToolSchedulerStatus {
    /// Returns admitted calls, including grants waiting to be observed by their caller.
    #[must_use]
    pub const fn active(self) -> usize {
        self.active
    }

    /// Returns calls still in the bounded admission queue.
    #[must_use]
    pub const fn waiting(self) -> usize {
        self.waiting
    }

    /// Returns resource keys held by currently admitted calls.
    #[must_use]
    pub const fn active_resource_keys(self) -> usize {
        self.active_resource_keys
    }

    /// Returns runtime lanes with active or waiting calls.
    #[must_use]
    pub const fn active_runtime_lanes(self) -> usize {
        self.active_runtime_lanes
    }
}

impl ToolScheduler {
    /// Creates a scheduler with a process-local global dispatch budget.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when `max_global` is zero.
    pub fn new(max_global: usize) -> Result<Self, ToolRuntimeError> {
        Self::with_waiter_limit(max_global, DEFAULT_GLOBAL_WAITERS)
    }

    /// Creates a scheduler with explicit global dispatch and waiter bounds.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when either bound is zero.
    pub fn with_waiter_limit(
        max_global: usize,
        max_waiters: usize,
    ) -> Result<Self, ToolRuntimeError> {
        if max_global == 0 {
            return Err(resource_error("global tool concurrency must be positive").into_portable());
        }
        if max_waiters == 0 {
            return Err(
                resource_error("global tool waiter capacity must be positive").into_portable(),
            );
        }
        Ok(Self::from_valid_bounds(max_global, max_waiters))
    }

    fn from_valid_bounds(max_global: usize, max_waiters: usize) -> Self {
        Self {
            inner: Arc::new(ToolSchedulerInner {
                state: Mutex::new(SchedulerState::default()),
                max_global,
                max_waiters,
            }),
        }
    }

    /// Returns the maximum number of admitted calls across runtimes sharing this scheduler.
    #[must_use]
    pub fn max_global(&self) -> usize {
        self.inner.max_global
    }

    /// Returns the bounded number of calls that may await global admission.
    #[must_use]
    pub fn max_waiters(&self) -> usize {
        self.inner.max_waiters
    }

    /// Reports whether two handles share the same admission domain.
    #[must_use]
    pub fn shares_budget_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns a bounded status snapshot without exposing queued resource names.
    #[must_use]
    pub fn status(&self) -> ToolSchedulerStatus {
        let state = lock(&self.inner.state);
        ToolSchedulerStatus {
            active: state.active_global,
            waiting: state.waiters.len(),
            active_resource_keys: state.active_resources.len(),
            active_runtime_lanes: state.runtimes.len(),
        }
    }

    pub(crate) fn runtime_lane(&self, config: ToolRuntimeConfig) -> RuntimeLane {
        let mut state = lock(&self.inner.state);
        let id = state.next_runtime_id;
        state.next_runtime_id = state.next_runtime_id.wrapping_add(1);
        RuntimeLane {
            scheduler: Arc::downgrade(&self.inner),
            id,
            capacity: config.max_concurrency,
            max_waiters: config.max_waiters,
        }
    }
}

pub(crate) fn default_scheduler() -> ToolScheduler {
    static DEFAULT_SCHEDULER: OnceLock<ToolScheduler> = OnceLock::new();
    DEFAULT_SCHEDULER
        .get_or_init(|| {
            ToolScheduler::from_valid_bounds(DEFAULT_GLOBAL_CONCURRENCY, DEFAULT_GLOBAL_WAITERS)
        })
        .clone()
}

/// Private-fielded per-runtime scheduler configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolRuntimeConfig {
    max_concurrency: usize,
    max_waiters: usize,
}

impl ToolRuntimeConfig {
    /// Creates a per-runtime (normally per-agent) dispatch budget.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when `max_concurrency` is zero.
    pub fn new(max_concurrency: usize) -> Result<Self, ToolRuntimeError> {
        Self::with_waiter_limit(max_concurrency, DEFAULT_RUNTIME_WAITERS)
    }

    /// Creates explicit per-runtime dispatch and waiter bounds.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when either bound is zero.
    pub fn with_waiter_limit(
        max_concurrency: usize,
        max_waiters: usize,
    ) -> Result<Self, ToolRuntimeError> {
        if max_concurrency == 0 {
            return Err(resource_error("runtime tool concurrency must be positive").into_portable());
        }
        if max_waiters == 0 {
            return Err(
                resource_error("runtime tool waiter capacity must be positive").into_portable(),
            );
        }
        Ok(Self {
            max_concurrency,
            max_waiters,
        })
    }

    /// Returns the runtime dispatch budget.
    #[must_use]
    pub const fn max_concurrency(self) -> usize {
        self.max_concurrency
    }

    /// Returns the bounded number of calls that may await this runtime's budget.
    #[must_use]
    pub const fn max_waiters(self) -> usize {
        self.max_waiters
    }
}

impl Default for ToolRuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrency: DEFAULT_RUNTIME_CONCURRENCY,
            max_waiters: DEFAULT_RUNTIME_WAITERS,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeLane {
    scheduler: Weak<ToolSchedulerInner>,
    id: u64,
    capacity: usize,
    max_waiters: usize,
}

pub(crate) struct SchedulerPermits {
    permit: Option<AdmissionPermit>,
}

impl SchedulerPermits {
    pub(crate) async fn acquire(
        scheduler: &ToolScheduler,
        runtime: &RuntimeLane,
        concurrency: &ToolConcurrency,
    ) -> Result<Self, ToolDispatchError> {
        let request = AdmissionRequest::new(runtime, concurrency)?;
        let permit = AdmissionFuture {
            scheduler: Arc::clone(&scheduler.inner),
            request: Some(request),
            ticket: None,
            runtime_waiter_limit: runtime.max_waiters,
        }
        .await?;
        Ok(Self {
            permit: Some(permit),
        })
    }
}

impl AdmissionRequest {
    fn new(
        runtime: &RuntimeLane,
        concurrency: &ToolConcurrency,
    ) -> Result<Self, ToolDispatchError> {
        let Some(scheduler) = runtime.scheduler.upgrade() else {
            return Err(resource_error("tool scheduler is no longer available"));
        };
        let (serial, mut resources) = match concurrency {
            ToolConcurrency::Serial => (true, Vec::new()),
            ToolConcurrency::Parallel => (false, Vec::new()),
            ToolConcurrency::ResourceExclusive(keys) => (false, keys.clone()),
            _ => {
                return Err(resource_error(
                    "unsupported tool concurrency policy cannot be scheduled",
                ));
            }
        };
        resources.sort();
        resources.dedup();
        drop(scheduler);
        Ok(Self {
            runtime_id: runtime.id,
            runtime_capacity: runtime.capacity,
            serial,
            resources,
        })
    }
}

struct AdmissionFuture {
    scheduler: Arc<ToolSchedulerInner>,
    request: Option<AdmissionRequest>,
    ticket: Option<u64>,
    runtime_waiter_limit: usize,
}

impl Future for AdmissionFuture {
    type Output = Result<AdmissionPermit, ToolDispatchError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let scheduler = Arc::clone(&self.scheduler);
        let mut state = lock(&scheduler.state);
        if self.ticket.is_none() {
            if state.waiters.len() + state.granted.len() >= scheduler.max_waiters {
                return Poll::Ready(Err(resource_error(
                    "tool scheduler waiter capacity was exhausted",
                )));
            }
            let Some(request) = self.request.take() else {
                return Poll::Ready(Err(resource_error(
                    "tool scheduler admission request was lost",
                )));
            };
            let usage = state.runtimes.entry(request.runtime_id).or_default();
            if usage.waiting >= self.runtime_waiter_limit {
                remove_unused_runtime(&mut state, request.runtime_id);
                return Poll::Ready(Err(resource_error(
                    "runtime tool waiter capacity was exhausted",
                )));
            }
            usage.waiting += 1;
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.waiters.push_back(Waiter {
                ticket,
                request,
                waker: context.waker().clone(),
            });
            self.ticket = Some(ticket);
        }

        let wakes = grant_waiters(&scheduler, &mut state);
        let Some(ticket) = self.ticket else {
            return Poll::Ready(Err(resource_error(
                "tool scheduler admission ticket was lost",
            )));
        };
        if let Some(allocation) = state.granted.remove(&ticket) {
            self.ticket = None;
            drop(state);
            wake_all(wakes);
            Poll::Ready(Ok(AdmissionPermit {
                scheduler,
                allocation: Some(allocation),
            }))
        } else {
            if let Some(waiter) = state
                .waiters
                .iter_mut()
                .find(|waiter| waiter.ticket == ticket)
                && !waiter.waker.will_wake(context.waker())
            {
                waiter.waker.clone_from(context.waker());
            }
            drop(state);
            wake_all(wakes);
            Poll::Pending
        }
    }
}

impl Drop for AdmissionFuture {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let mut state = lock(&self.scheduler.state);
        let wakes = if let Some(index) = state
            .waiters
            .iter()
            .position(|waiter| waiter.ticket == ticket)
        {
            let Some(waiter) = state.waiters.remove(index) else {
                debug_assert!(false, "the located waiter is present");
                return;
            };
            let usage = state.runtimes.entry(waiter.request.runtime_id).or_default();
            usage.waiting = usage.waiting.saturating_sub(1);
            remove_unused_runtime(&mut state, waiter.request.runtime_id);
            grant_waiters(&self.scheduler, &mut state)
        } else if let Some(allocation) = state.granted.remove(&ticket) {
            release_allocation(&allocation, &mut state);
            grant_waiters(&self.scheduler, &mut state)
        } else {
            Vec::new()
        };
        drop(state);
        wake_all(wakes);
    }
}

struct AdmissionPermit {
    scheduler: Arc<ToolSchedulerInner>,
    allocation: Option<Allocation>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let Some(allocation) = self.allocation.take() else {
            return;
        };
        let mut state = lock(&self.scheduler.state);
        release_allocation(&allocation, &mut state);
        let wakes = grant_waiters(&self.scheduler, &mut state);
        drop(state);
        wake_all(wakes);
    }
}

impl Drop for SchedulerPermits {
    fn drop(&mut self) {
        drop(self.permit.take());
    }
}

fn grant_waiters(scheduler: &ToolSchedulerInner, state: &mut SchedulerState) -> Vec<Waker> {
    let mut wakes = Vec::new();
    while let Some(index) =
        (0..state.waiters.len()).find(|index| request_is_admissible(scheduler, state, *index))
    {
        let Some(waiter) = state.waiters.remove(index) else {
            debug_assert!(false, "the selected waiter is present");
            break;
        };
        let usage = state.runtimes.entry(waiter.request.runtime_id).or_default();
        usage.waiting = usage.waiting.saturating_sub(1);
        usage.active += 1;
        if waiter.request.serial {
            usage.serial = true;
        }
        state.active_global += 1;
        for key in &waiter.request.resources {
            let inserted = state.active_resources.insert(key.clone());
            debug_assert!(inserted, "admission never grants an active resource twice");
        }
        let previous = state.granted.insert(
            waiter.ticket,
            Allocation {
                runtime_id: waiter.request.runtime_id,
                serial: waiter.request.serial,
                resources: waiter.request.resources,
            },
        );
        debug_assert!(previous.is_none(), "tickets are unique while retained");
        wakes.push(waiter.waker);
    }
    wakes
}

fn request_is_admissible(
    scheduler: &ToolSchedulerInner,
    state: &SchedulerState,
    index: usize,
) -> bool {
    if state.active_global >= scheduler.max_global {
        return false;
    }
    let request = &state.waiters[index].request;
    let usage = state.runtimes.get(&request.runtime_id);
    let active = usage.map_or(0, |usage| usage.active);
    let serial_active = usage.is_some_and(|usage| usage.serial);
    if request.serial {
        if active != 0 {
            return false;
        }
    } else if serial_active || active >= request.runtime_capacity {
        return false;
    }
    if request
        .resources
        .iter()
        .any(|key| state.active_resources.contains(key))
    {
        return false;
    }
    !state.waiters.iter().take(index).any(|earlier| {
        resource_sets_intersect(&earlier.request.resources, &request.resources)
            || (earlier.request.runtime_id == request.runtime_id
                && (earlier.request.serial || request.serial))
    })
}

fn resource_sets_intersect(left: &[ToolResourceKey], right: &[ToolResourceKey]) -> bool {
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

fn release_allocation(allocation: &Allocation, state: &mut SchedulerState) {
    state.active_global = state.active_global.saturating_sub(1);
    if let Some(usage) = state.runtimes.get_mut(&allocation.runtime_id) {
        usage.active = usage.active.saturating_sub(1);
        if allocation.serial {
            usage.serial = false;
        }
    } else {
        debug_assert!(false, "active runtime usage is present");
    }
    for key in &allocation.resources {
        let removed = state.active_resources.remove(key);
        debug_assert!(removed, "released resources were active");
    }
    remove_unused_runtime(state, allocation.runtime_id);
}

fn remove_unused_runtime(state: &mut SchedulerState, runtime_id: u64) {
    if state
        .runtimes
        .get(&runtime_id)
        .is_some_and(|usage| usage.active == 0 && usage.waiting == 0)
    {
        state.runtimes.remove(&runtime_id);
    }
}

fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

/// Owned future returned by an object-safe runtime timer.
pub type ToolTimerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Object-safe monotonic timer seam used for deadlines and cleanup grace periods.
pub trait ToolTimer: Send + Sync + 'static {
    /// Returns a future completing at or after `deadline`.
    fn sleep_until(&self, deadline: Instant) -> ToolTimerFuture;
}

/// Tokio-backed monotonic timer for the supported runtime foundation.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioToolTimer {
    _private: (),
}

impl TokioToolTimer {
    /// Creates a Tokio timer. Polling sleeps requires an active Tokio runtime.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl ToolTimer for TokioToolTimer {
    fn sleep_until(&self, deadline: Instant) -> ToolTimerFuture {
        Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
            deadline,
        )))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn resource_error(message: &'static str) -> ToolDispatchError {
    ToolDispatchError::new(ToolRuntimeErrorKind::ResourceLimit, message)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use futures::task::noop_waker;

    use super::*;

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(&noop_waker()))
    }

    fn resource(key: &str) -> Result<ToolConcurrency, ToolRuntimeError> {
        Ok(ToolConcurrency::ResourceExclusive(vec![
            ToolResourceKey::new(key).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "test resource key is invalid",
                    &error,
                )
            })?,
        ]))
    }

    #[test]
    fn admission_is_atomic_and_disjoint_work_bypasses_a_blocked_resource()
    -> Result<(), Box<dyn std::error::Error>> {
        let scheduler = ToolScheduler::with_waiter_limit(2, 8)?;
        let first_lane = scheduler.runtime_lane(ToolRuntimeConfig::new(2)?);
        let second_lane = scheduler.runtime_lane(ToolRuntimeConfig::new(1)?);
        let held = futures::executor::block_on(SchedulerPermits::acquire(
            &scheduler,
            &first_lane,
            &resource("resource:held")?,
        ))?;
        let blocked_policy = resource("resource:held")?;
        let mut blocked = Box::pin(SchedulerPermits::acquire(
            &scheduler,
            &first_lane,
            &blocked_policy,
        ));
        assert!(poll_once(blocked.as_mut()).is_pending());
        assert_eq!(scheduler.status().active(), 1);

        let disjoint = futures::executor::block_on(SchedulerPermits::acquire(
            &scheduler,
            &second_lane,
            &ToolConcurrency::Parallel,
        ))?;
        assert_eq!(scheduler.status().active(), 2);
        drop(disjoint);
        drop(blocked);
        drop(held);
        Ok(())
    }

    #[test]
    fn cancellation_removes_a_queued_request_and_wakes_the_chain()
    -> Result<(), Box<dyn std::error::Error>> {
        let scheduler = ToolScheduler::with_waiter_limit(1, 4)?;
        let lane = scheduler.runtime_lane(ToolRuntimeConfig::new(1)?);
        let held = futures::executor::block_on(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ))?;
        let mut first = Box::pin(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ));
        let mut second = Box::pin(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ));
        assert!(poll_once(first.as_mut()).is_pending());
        assert!(poll_once(second.as_mut()).is_pending());
        drop(first);
        assert_eq!(scheduler.status().waiting(), 1);
        drop(held);
        assert!(matches!(poll_once(second.as_mut()), Poll::Ready(Ok(_))));
        Ok(())
    }

    #[test]
    fn waiter_capacity_and_resource_state_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let scheduler = ToolScheduler::with_waiter_limit(1, 1)?;
        let lane = scheduler.runtime_lane(ToolRuntimeConfig::with_waiter_limit(1, 1)?);
        for index in 0..128 {
            let policy = resource(&format!("resource:{index}"))?;
            let permit =
                futures::executor::block_on(SchedulerPermits::acquire(&scheduler, &lane, &policy))?;
            assert_eq!(scheduler.status().active_resource_keys(), 1);
            drop(permit);
            assert_eq!(scheduler.status().active_resource_keys(), 0);
        }
        let held = futures::executor::block_on(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ))?;
        let mut waiting = Box::pin(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ));
        let mut overflow = Box::pin(SchedulerPermits::acquire(
            &scheduler,
            &lane,
            &ToolConcurrency::Parallel,
        ));
        assert!(poll_once(waiting.as_mut()).is_pending());
        assert!(matches!(
            poll_once(overflow.as_mut()),
            Poll::Ready(Err(error)) if error.kind() == ToolRuntimeErrorKind::ResourceLimit
        ));
        drop(waiting);
        drop(held);
        assert_eq!(scheduler.status().active_runtime_lanes(), 0);
        Ok(())
    }

    #[test]
    fn default_scheduler_is_process_shared_but_explicit_instances_are_isolated()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = default_scheduler();
        let second = default_scheduler();
        assert!(first.shares_budget_with(&second));
        assert!(!first.shares_budget_with(&ToolScheduler::new(64)?));
        Ok(())
    }
}
