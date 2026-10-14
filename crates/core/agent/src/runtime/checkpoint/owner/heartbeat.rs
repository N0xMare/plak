//! Joined periodic lease renewal owned by the checkpoint owner.

use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::checkpoint::owner::{
    CheckpointAuthority, DurableBranchContext, lease_authority_lost, lease_taken_over,
};
use crate::runtime::terminal::lock_mutex;
use crate::{AgentError, AgentErrorKind, SessionStoreError};
use plakous_model::CancellationToken;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
pub(super) struct LeaseHeartbeat {
    stop: CancellationToken,
    lost: CancellationToken,
    failure: Arc<Mutex<Option<AgentError>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        // Abandonment backstop only; ordinary shutdown still joins the task.
        self.stop.cancel();
    }
}

impl LeaseHeartbeat {
    pub(super) fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }
    pub(super) fn lost(&self) -> &CancellationToken {
        &self.lost
    }

    pub(super) fn start(authority: Arc<Mutex<CheckpointAuthority>>) -> Self {
        let stop = CancellationToken::new();
        let lost = CancellationToken::new();
        let failure = Arc::new(Mutex::new(None));
        let durable = lock_mutex(&authority).durable.clone();
        let task = durable.map(|branch| {
            let stop_task = stop.clone();
            let lost_task = lost.clone();
            let failure_task = Arc::clone(&failure);
            let interval = heartbeat_interval(branch.lease.lease_seconds());
            tokio::spawn(async move {
                let mut next_renewal = tokio::time::Instant::now() + interval;
                loop {
                    tokio::select! {
                        // Biased: once cleanup has requested the stop, the
                        // heartbeat must not start one more renewal.
                        biased;
                        () = stop_task.cancelled() => return,
                        () = tokio::time::sleep_until(next_renewal) => {}
                    }
                    // Stores stamp expiry before returning the acknowledgement.
                    // A slow acknowledgement must not add another full sleep;
                    // anchor at this start rather than replaying missed ticks.
                    next_renewal = tokio::time::Instant::now() + interval;
                    let error = match renew_once(&authority).await {
                        Ok(()) => continue,
                        // A busy or slow store does not end authority while
                        // the held lease is live: retry sooner instead.
                        Err(RenewalFailure::Transient(_)) if lease_live(&authority) => {
                            next_renewal = tokio::time::Instant::now() + retry_delay(interval);
                            continue;
                        }
                        Err(RenewalFailure::Transient(error) | RenewalFailure::Refused(error)) => {
                            error
                        }
                    };
                    *lock_mutex(&failure_task) = Some(error);
                    lost_task.cancel();
                    return;
                }
            })
        });
        Self {
            stop,
            lost,
            failure,
            task,
        }
    }

    pub(super) fn check(&self) -> Result<(), AgentError> {
        if let Some(error) = lock_mutex(&self.failure).clone() {
            return Err(error);
        }
        if self.lost.is_cancelled() {
            return Err(lease_authority_lost());
        }
        Ok(())
    }

    pub(super) fn failure(&self) -> AgentError {
        lock_mutex(&self.failure)
            .clone()
            .unwrap_or_else(lease_authority_lost)
    }

    pub(super) async fn stop_and_join(&mut self, timeout: Duration) -> Result<(), AgentError> {
        self.stop.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(AgentError::new(
                        AgentErrorKind::Internal,
                        "durable lease heartbeat task did not join cleanly",
                    ));
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "durable lease heartbeat cleanup exceeded its configured deadline",
                    ));
                }
            }
        }
        self.check()
    }
}

fn heartbeat_interval(lease_seconds: NonZeroU64) -> Duration {
    let millis = lease_seconds.get().saturating_mul(1_000).saturating_div(3);
    Duration::from_millis(millis.max(1))
}

fn retry_delay(interval: Duration) -> Duration {
    (interval / 4).max(Duration::from_millis(1))
}

/// Why one renewal attempt failed.
enum RenewalFailure {
    /// The store was busy or unavailable, or a store or gate deadline elapsed:
    /// the held lease may still be live.
    Transient(AgentError),
    /// The store refused this owner, or its authority moved underneath it.
    Refused(AgentError),
}

impl RenewalFailure {
    fn from_store(error: StoreCallError) -> Self {
        if error.is_retryable() {
            Self::Transient(error.into_agent())
        } else {
            Self::Refused(error.into_agent())
        }
    }

    fn into_agent(self) -> AgentError {
        match self {
            Self::Transient(error) | Self::Refused(error) => error,
        }
    }
}

/// Whether the store clock still reads before the held lease's expiry. An
/// unreadable clock counts as expired.
fn lease_live(authority: &Arc<Mutex<CheckpointAuthority>>) -> bool {
    let authority = lock_mutex(authority);
    authority.durable.as_ref().is_some_and(|branch| {
        branch.store.lease_now().is_ok_and(|now| {
            now.same_domain_cmp(branch.lease.expires_at())
                .is_ok_and(|order| order == std::cmp::Ordering::Less)
        })
    })
}

#[cfg(test)]
pub(super) async fn renew_shared_durable_lease(
    authority: &Arc<Mutex<CheckpointAuthority>>,
) -> Result<(), AgentError> {
    renew_once(authority)
        .await
        .map_err(RenewalFailure::into_agent)
}

async fn renew_once(authority: &Arc<Mutex<CheckpointAuthority>>) -> Result<(), RenewalFailure> {
    let initial = lock_mutex(authority).durable.clone();
    let Some(initial) = initial else {
        return Ok(());
    };
    let _gate = tokio::time::timeout(initial.store_timeouts.operation, initial.lease_gate.lock())
        .await
        .map_err(|_| {
            RenewalFailure::Transient(AgentError::new(
                AgentErrorKind::Session,
                "durable lease-renew gate exceeded its configured deadline",
            ))
        })?;
    renew_under_gate(authority).await?;
    Ok(())
}

// The caller holds the branch lease gate across renewal and use of this exact
// acknowledged authority, including checkpoint commit and receipt recovery.
pub(super) async fn renew_shared_durable_lease_under_gate(
    authority: &Arc<Mutex<CheckpointAuthority>>,
) -> Result<DurableBranchContext, AgentError> {
    renew_under_gate(authority)
        .await
        .map_err(RenewalFailure::into_agent)
}

async fn renew_under_gate(
    authority: &Arc<Mutex<CheckpointAuthority>>,
) -> Result<DurableBranchContext, RenewalFailure> {
    let current = lock_mutex(authority).durable.clone().ok_or_else(|| {
        RenewalFailure::Refused(AgentError::new(
            AgentErrorKind::Internal,
            "durable branch context disappeared",
        ))
    })?;
    let renewed = match store_call(
        StorePhase::Renew,
        current.store_timeouts.operation,
        current.store.renew(current.lease.clone()),
    )
    .await
    {
        Ok(renewed) => renewed,
        // The holder outlived its lease without being replaced: a laptop
        // asleep, a stopped process. The store revives it under the same fence
        // only when nobody acquired the branch in between; otherwise this
        // owner has been superseded and stops. A revival the store could not
        // answer is not a takeover and keeps its own error.
        Err(error) if error.is(SessionStoreError::LeaseExpired) => match store_call(
            StorePhase::Renew,
            current.store_timeouts.operation,
            current.store.revive(current.lease.clone()),
        )
        .await
        {
            Ok(revived) => revived,
            Err(error) if error.is_retryable() => {
                return Err(RenewalFailure::Transient(error.into_agent()));
            }
            Err(_) => return Err(RenewalFailure::Refused(lease_taken_over())),
        },
        Err(error) => return Err(RenewalFailure::from_store(error)),
    };
    let mut authority = lock_mutex(authority);
    let active = authority.durable.as_mut().ok_or_else(|| {
        RenewalFailure::Refused(AgentError::new(
            AgentErrorKind::Internal,
            "durable branch context disappeared",
        ))
    })?;
    if active.head != current.head || active.lease != current.lease {
        return Err(RenewalFailure::Refused(AgentError::new(
            AgentErrorKind::Session,
            "durable lease renewal raced checkpoint authority",
        )));
    }
    active.lease = renewed;
    Ok(active.clone())
}
