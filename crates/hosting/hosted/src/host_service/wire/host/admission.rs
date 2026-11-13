//! Principal-wide console budgets shared by every connection of a host.

use super::reads::HostPrincipal;
use crate::error::{CliFailure, CliResult};
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

/// A reversible host-upgrade admission pause. It never cancels running work.
pub struct AdmissionPause {
    pub(super) gate: Arc<std::sync::RwLock<bool>>,
    committed: Arc<std::sync::atomic::AtomicBool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AdmissionPause {
    /// Linearizes surrender against an existing-session stop, without opening ingress.
    ///
    /// # Errors
    /// An operator control or requester loss cancelled preparation.
    pub fn commit(&self) -> CliResult<()> {
        let _gate = self.gate.write().unwrap_or_else(PoisonError::into_inner);
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(CliFailure::new("host upgrade cancelled before surrender"));
        }
        self.committed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

impl Drop for AdmissionPause {
    fn drop(&mut self) {
        *self.gate.write().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> super::SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    plakous_host_protocol::wire::HostFrame<P>: serde::Serialize,
    plakous_host_protocol::wire::ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Refuses new client requests after previously admitted frame handlers
    /// return. Running work continues. Dropping the permit restores admissions.
    ///
    /// # Errors
    /// Another upgrade already paused this host, or the host is stopping.
    pub fn pause_admissions(&self) -> CliResult<AdmissionPause> {
        self.pause_admissions_for_upgrade(Arc::default())
    }

    /// Pauses ingress with the coordinator's cancellation flag. Existing-session
    /// stop/interrupt requests cancel reversible preparation before ordinary handling.
    ///
    /// # Errors
    /// Another pause exists or the host is stopping.
    pub fn pause_admissions_for_upgrade(
        &self,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> CliResult<AdmissionPause> {
        let mut paused = self
            .upgrade_admission
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if *paused || self.closed() {
            return Err(CliFailure::new("the host is already upgrading or stopping"));
        }
        *paused = true;
        self.upgrade_committed
            .store(false, std::sync::atomic::Ordering::Release);
        *self
            .upgrade_cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&cancelled));
        Ok(AdmissionPause {
            gate: Arc::clone(&self.upgrade_admission),
            committed: Arc::clone(&self.upgrade_committed),
            cancelled,
        })
    }

    /// Stops the host, so no session opens or is created from here on, then
    /// releases every resident session as a planned handoff, all at once,
    /// each joined within `deadline`; the sessions that did not release in
    /// time, or failed to, with why.
    pub fn hand_off_all(&self, deadline: Duration) -> Vec<(String, CliFailure)> {
        // Closed first, so the slots below are every session this host holds.
        self.close();
        self.hand_off_residents(deadline)
    }

    /// Drains already accepted mailboxes and releases residents under an
    /// upgrade pause, without permanently closing the session directory.
    /// Failed preparation can drop the pause and open the old sessions again.
    ///
    /// # Errors
    /// The permit belongs to another host or admissions are not paused.
    pub fn hand_off_paused(
        &self,
        pause: &AdmissionPause,
        deadline: Duration,
    ) -> CliResult<Vec<(String, CliFailure)>> {
        if !Arc::ptr_eq(&pause.gate, &self.upgrade_admission)
            || !*self
                .upgrade_admission
                .read()
                .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(CliFailure::new(
                "host handoff requires its own upgrade admission pause",
            ));
        }
        Ok(self.hand_off_residents(deadline))
    }

    fn hand_off_residents(&self, deadline: Duration) -> Vec<(String, CliFailure)> {
        let slots = self.slots();
        std::thread::scope(|scope| {
            let handles: Vec<_> = slots
                .iter()
                .map(|slot| scope.spawn(move || (slot.known(), slot.hand_off(deadline))))
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| match handle.join() {
                    Ok((_, Ok(()))) => None,
                    Ok((session, Err(failure))) => Some((session, failure)),
                    Err(_) => Some((
                        String::new(),
                        CliFailure::new("a session's handoff panicked"),
                    )),
                })
                .collect()
        })
    }
}
