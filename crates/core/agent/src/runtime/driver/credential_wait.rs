//! Cooperative credential parking, never dispatch or persistence authority.

use super::Driver;
use crate::runtime::terminal::cancelled_error;
use crate::{AgentError, AgentEventKind, EventContext};
use plakous_model::{
    LanguageModel, ModelError, ModelErrorPhase, ModelPreparationError, RuntimeModelAuthority,
    RuntimeModelAuthorityError, Timestamp,
};
use std::sync::Arc;

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
        if let Some(context) = self.context {
            let _ = self.events.publish(
                context,
                AgentEventKind::ModelCredentialWait { waiting: false },
            );
        }
    }
}

impl Driver {
    pub(super) async fn request_authority(
        &mut self,
        model: Arc<dyn LanguageModel>,
        mut checked_at: Timestamp,
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
            match Arc::clone(&model)
                .revalidate_runtime_authority(checked_at)
                .await
            {
                Ok(authority) => return Ok((authority, checked_at)),
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
            let mut delay = std::time::Duration::from_millis(250);
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
                        delay = (delay * 2).min(std::time::Duration::from_secs(2));
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
                () = crate::executor::sleep(std::time::Duration::from_millis(100)), if active.is_none() => {
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
