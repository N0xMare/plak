//! Cooperative credential parking, never dispatch or persistence authority.

use super::Driver;
use crate::runtime::PromptCommand;
use crate::runtime::terminal::cancelled_error;
use crate::{AgentError, AgentEventKind, EventContext};
use plakous_model::{
    LanguageModel, ModelError, ModelErrorPhase, ModelPreparationError, RuntimeModelAuthority,
    RuntimeModelAuthorityError, Timestamp, authority_acquisition,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;

const AUTHORITY_ACQUISITION_WINDOW: Duration = Duration::from_secs(60);
type RoundAuthority = Result<(RuntimeModelAuthority, Timestamp), RuntimeModelAuthorityError>;

struct Waiting {
    state: Arc<std::sync::Mutex<Option<crate::runtime::CredentialWait>>>,
    events: crate::EventPipeline,
    context: Option<EventContext>,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if let Some(context) = self.context
            && let Err(error) = self.events.publish(
                context,
                AgentEventKind::ModelCredentialWait { waiting: false },
            )
        {
            // The host never sees a clear the hub refused; the gate's build
            // names it.
            crate::test_hooks::credential_wait_clear_refused(&error);
        }
    }
}

impl Driver {
    pub(super) async fn request_authority(
        &mut self,
        model: Arc<dyn LanguageModel>,
        checked_at: Timestamp,
    ) -> Result<(RuntimeModelAuthority, Timestamp), AgentError> {
        self.authority_for_request(model, checked_at, false).await
    }

    pub(super) async fn round_request_authority(
        &mut self,
        model: Arc<dyn LanguageModel>,
        checked_at: Timestamp,
    ) -> Result<(RuntimeModelAuthority, Timestamp), AgentError> {
        self.authority_for_request(model, checked_at, true).await
    }

    async fn authority_for_request(
        &mut self,
        model: Arc<dyn LanguageModel>,
        mut checked_at: Timestamp,
        turn_round: bool,
    ) -> Result<(RuntimeModelAuthority, Timestamp), AgentError> {
        loop {
            if self.shutdown.is_cancelled()
                || self
                    .active_root
                    .as_ref()
                    .is_some_and(|root| root.cancellation.is_cancelled())
            {
                return Err(cancelled_error());
            }
            let result = if turn_round {
                self.check_round_authority(Arc::clone(&model), checked_at, &mut None)
                    .await?
            } else {
                Arc::clone(&model)
                    .revalidate_runtime_authority(checked_at)
                    .await
                    .map(|authority| (authority, checked_at))
            };
            match result {
                Ok(authority) => return Ok(authority),
                Err(RuntimeModelAuthorityError::AwaitingCredential) => {
                    let cancellation = self
                        .active_root
                        .as_ref()
                        .map_or_else(|| self.shutdown.clone(), |root| root.cancellation.clone());
                    self.wait_for_credential(Arc::clone(&model), cancellation)
                        .await?;
                    checked_at = self.clock.now();
                }
                Err(error) => return Err(self.authority_error(error)),
            }
        }
    }

    /// Checks a turn round before dispatch, while still accepting its
    /// cancellation command. The paid compaction path never uses this scope.
    pub(super) async fn check_round_authority(
        &mut self,
        model: Arc<dyn LanguageModel>,
        checked_at: Timestamp,
        dispatch_guard: &mut Option<OwnedMutexGuard<()>>,
    ) -> Result<RoundAuthority, AgentError> {
        let active = self.active_root.clone();
        let active = active.as_ref();
        let mut check = authority_acquisition::Check::new(
            model.revalidate_runtime_authority(checked_at),
            AUTHORITY_ACQUISITION_WINDOW,
        );
        loop {
            match self.during_round_authority(active, check.step()).await? {
                authority_acquisition::Step::Break((result, waited)) => {
                    let now = if waited { self.clock.now() } else { checked_at };
                    return Ok(result.map(|authority| (authority, now)));
                }
                authority_acquisition::Step::Continue(backoff) => {
                    if !self
                        .park_round_authority(active, backoff, dispatch_guard)
                        .await?
                    {
                        return Ok(Err(RuntimeModelAuthorityError::AcquisitionTimedOut));
                    }
                }
            }
        }
    }

    /// Release admission only at an unsent retry boundary, and take it back
    /// before polling the same acquisition. Do not rerun model revalidation:
    /// it might already have performed another credential or catalog call.
    async fn park_round_authority(
        &mut self,
        active: Option<&PromptCommand>,
        backoff: authority_acquisition::Backoff,
        dispatch_guard: &mut Option<OwnedMutexGuard<()>>,
    ) -> Result<bool, AgentError> {
        let need_slot = self.request_slot.take().is_some();
        let need_gate = dispatch_guard.take().is_some();
        let started = crate::executor::now();
        self.during_round_authority(active, crate::executor::sleep(backoff.delay))
            .await?;
        let remaining = backoff.remaining.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(false);
        }
        let restore = async {
            if need_slot && let Some(command) = active {
                self.request_slot = self.take_request_slot(command).await?;
            }
            if need_gate {
                let gate = Arc::clone(&self.provider_dispatch_gate);
                *dispatch_guard = Some(
                    self.during_round_authority(active, gate.lock_owned())
                        .await?,
                );
            }
            Ok(())
        };
        match crate::executor::timeout(remaining, restore).await {
            Ok(result) => result.map(|()| true),
            Err(_) => Ok(false),
        }
    }

    async fn during_round_authority<T>(
        &mut self,
        active: Option<&PromptCommand>,
        work: impl Future<Output = T>,
    ) -> Result<T, AgentError> {
        let cancellation =
            active.map_or_else(|| self.shutdown.clone(), |root| root.cancellation.clone());
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(cancelled_error()),
                () = self.shutdown.cancelled() => return Err(cancelled_error()),
                () = self.authority.lease_lost() => return Err(self.authority.heartbeat_failure()),
                result = &mut work => return Ok(result),
                control = self.control_receiver.recv() => {
                    let Some(control) = control else { return Err(cancelled_error()); };
                    Box::pin(self.handle_control(active, control)).await;
                }
            }
        }
    }

    pub(super) fn authority_error(&self, error: RuntimeModelAuthorityError) -> AgentError {
        self.project_model_error(
            ModelError::from(ModelPreparationError::from(error)),
            ModelErrorPhase::Preparation,
        )
    }

    pub(super) async fn wait_for_credential(
        &mut self,
        model: Arc<dyn LanguageModel>,
        cancellation: plakous_model::CancellationToken,
    ) -> Result<(), AgentError> {
        // A waiting request must not starve another agent's inference.
        self.request_slot.take();
        let active = self.active_root.clone();
        *self
            .credential_wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            active
                .as_ref()
                .map_or(crate::runtime::CredentialWait::SessionWorkflow, |root| {
                    crate::runtime::CredentialWait::Turn(root.turn_id)
                }),
        );
        let context = active.as_ref().map(|root| EventContext::turn(root.turn_id));
        if let Some(context) = context {
            let _ = self.events.publish(
                context,
                AgentEventKind::ModelCredentialWait { waiting: true },
            );
        }
        let _waiting = Waiting {
            state: Arc::clone(&self.credential_wait),
            events: self.events.clone(),
            context,
        };
        let started = crate::executor::now();
        let clock = Arc::clone(&self.clock);
        let mut ready = Box::pin(async move {
            let mut delay = Duration::from_millis(250);
            loop {
                Arc::clone(&model).wait_for_runtime_authority().await?;
                match Arc::clone(&model)
                    .revalidate_runtime_authority(clock.now())
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(RuntimeModelAuthorityError::AwaitingCredential) => {
                        // A readiness edge can race another logout or refresh.
                        // Keep one visible park and bound even a spurious wake.
                        crate::executor::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(2));
                    }
                    Err(error) => return Err(error),
                }
            }
        });
        let interruption = self.authority.reader();
        let result = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(cancelled_error()),
                () = self.shutdown.cancelled() => break Err(cancelled_error()),
                () = self.authority.lease_lost() => break Err(self.authority.heartbeat_failure()),
                result = &mut ready => break result.map_err(|error| self.authority_error(error)),
                () = crate::executor::sleep(Duration::from_millis(100)), if active.is_none() => {
                    if interruption.cooperative_wait_interrupted() { break Err(cancelled_error()); }
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else { break Err(cancelled_error()); };
                    Box::pin(self.handle_control(active.as_ref(), control)).await;
                }
            }
        };
        self.credential_wait_elapsed = self
            .credential_wait_elapsed
            .saturating_add(started.elapsed());
        result
    }
}
