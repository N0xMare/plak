//! The between-operations boundary that swaps in a fresh exact resolution.

use crate::runtime::PromptCommand;
use crate::runtime::driver::Driver;
use crate::runtime::terminal::cancelled_error;
use crate::{AgentError, AgentErrorKind, TelemetryObservationKind};
use std::sync::Arc;

impl Driver {
    /// Past the admitted evidence window of the current resolution, swap in a
    /// fresh exact resolution of the same model before the next model
    /// operation. Callers run this only between model operations, so no
    /// prepared call is outstanding and none is re-targeted.
    ///
    /// Inside the window, and for a model installed without a reresolver, this
    /// does nothing: no revalidation, resolution or permit. A refusal ends the
    /// turn with the reresolver's remedy before anything is dispatched.
    pub(super) async fn reresolve_expired_model(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
        let Some(rebind) = self.model_rebind.as_ref() else {
            return Ok(());
        };
        if self.clock.now() < rebind.admitted_until {
            return Ok(());
        }
        let operation = Arc::clone(&rebind.reresolver).reresolve(rebind.admitted_until);
        let fresh = tokio::select! {
            biased;
            () = command.cancellation.cancelled() => return Err(cancelled_error()),
            () = self.shutdown.cancelled() => return Err(cancelled_error()),
            fresh = operation => fresh,
        };
        let checked_at = self.clock.now();
        let admitted = match fresh {
            Ok(fresh) => self
                .model_rebind
                .as_mut()
                .ok_or("re-resolution state is missing")
                .and_then(|rebind| rebind.admit(&self.provider_configuration, fresh, checked_at))
                .map_err(|reason| {
                    format!(
                        "{reason}; the model's admitted evidence window closed and only an explicit rebind can continue"
                    )
                }),
            Err(error) => Err(error.message().to_owned()),
        };
        self.emit_telemetry(
            self.telemetry_context.with_turn(command.turn_id),
            TelemetryObservationKind::ModelReresolved {
                accepted: admitted.is_ok(),
            },
        );
        let (model, provider_configuration) = admitted.map_err(|message| {
            AgentError::new(
                AgentErrorKind::ExternalBindingUnavailable,
                format!("model re-resolution refused: {message}"),
            )
        })?;
        if let Some(rebind) = self.model_rebind.as_ref() {
            rebind.reresolver.accepted(rebind.admitted_until);
        }
        self.model = model;
        self.provider_configuration = provider_configuration;
        Ok(())
    }
}
