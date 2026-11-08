//! Principal-wide console budgets shared by every connection of a host.

use super::reads::HostPrincipal;
use plakous_host_protocol::wire::RequestRefusal;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// The product's one source for welcome limits and generic read enforcement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostReadLimits {
    /// Checked before decoding the product request payload.
    pub request_bytes: usize,
    /// Active requests on one connection.
    pub per_connection: usize,
    /// Active requests for one identity across connections.
    pub per_principal: usize,
    /// Active requests across the entire host.
    pub host: usize,
    /// Whole-request deadline, including scheduling and I/O waits.
    pub deadline: Duration,
    /// Console messages per second, independent of session traffic.
    pub messages_per_second: u32,
    /// Principal-wide token bucket capacity.
    pub burst: u32,
}

pub(super) struct Admission {
    pub(super) limits: HostReadLimits,
    pub(super) host: Arc<Semaphore>,
    principals: Mutex<BTreeMap<HostPrincipal, Arc<PrincipalBudget>>>,
}

impl Admission {
    pub(super) fn new(limits: HostReadLimits) -> Self {
        Self {
            limits,
            host: Arc::new(Semaphore::new(limits.host.min(Semaphore::MAX_PERMITS))),
            principals: Mutex::default(),
        }
    }
    pub(super) fn principal(&self, principal: &HostPrincipal) -> Arc<PrincipalBudget> {
        let mut principals = self
            .principals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A disconnected identity keeps its consumed bucket until it refills;
        // reconnect must not mint a fresh burst. Idle, replenished entries retire.
        principals
            .retain(|_, value| Arc::strong_count(value) > 1 || !value.replenished(self.limits));
        if let Some(budget) = principals.get(principal) {
            return budget.clone();
        }
        let budget = Arc::new(PrincipalBudget {
            active: Arc::new(Semaphore::new(
                self.limits.per_principal.min(Semaphore::MAX_PERMITS),
            )),
            rate: Mutex::new(Rate {
                tokens: f64::from(self.limits.burst),
                updated: Instant::now(),
            }),
        });
        principals.insert(principal.clone(), budget.clone());
        budget
    }
}

pub(super) struct PrincipalBudget {
    pub(super) active: Arc<Semaphore>,
    rate: Mutex<Rate>,
}
struct Rate {
    tokens: f64,
    updated: Instant,
}

impl PrincipalBudget {
    fn replenished(&self, limits: HostReadLimits) -> bool {
        let mut rate = self.rate.lock().unwrap_or_else(PoisonError::into_inner);
        rate.refill(limits);
        rate.tokens >= f64::from(limits.burst)
            && self.active.available_permits() == limits.per_principal
    }
    pub(super) fn rate(&self, limits: HostReadLimits) -> Result<(), RequestRefusal> {
        let mut rate = self.rate.lock().unwrap_or_else(PoisonError::into_inner);
        rate.refill(limits);
        if rate.tokens >= 1.0 {
            rate.tokens -= 1.0;
            return Ok(());
        }
        let wait = if limits.messages_per_second == 0 {
            limits.deadline
        } else {
            Duration::from_secs_f64((1.0 - rate.tokens) / f64::from(limits.messages_per_second))
        };
        Err(RequestRefusal::RateLimited {
            retry_after_ms: u64::try_from(wait.as_millis().saturating_add(1)).unwrap_or(u64::MAX),
        })
    }
}

impl Rate {
    fn refill(&mut self, limits: HostReadLimits) {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.updated).as_secs_f64()
                * f64::from(limits.messages_per_second))
        .min(f64::from(limits.burst));
        self.updated = now;
    }
}
