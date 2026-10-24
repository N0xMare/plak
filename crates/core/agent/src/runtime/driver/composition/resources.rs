//! Evaluator retention after a cell ends, and broker events.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the composition driver that shares its vocabulary"
)]

use super::*;

impl Driver {
    /// Retains or retires the evaluator behind a cell that just ended.
    ///
    /// A healthy generation is restored for the next cell. A generation that
    /// died with its own definite outcome is joined here instead, under a typed
    /// reset: the cell's result is already known, so it is delivered rather
    /// than turned into recovery, and the next cell starts a fresh generation
    /// without re-evaluating any source.
    pub(super) async fn settle_terminal_local_composition_resources(
        &self,
        turn_id: TurnId,
        state: &mut CompositionOwner,
        resources: InterpreterResources,
    ) -> Result<(), AgentError> {
        if !resources.generation_lost() {
            state.restore_terminal_resources(resources);
            return Ok(());
        }
        state.retire_cell();
        let mut cleanup = Vec::new();
        if let Err(observation) = observe_local_composition_session_loss(
            state,
            &LocalCompositionSessionLoss::ProtocolViolation(
                "local composition evaluator generation ended with its cell".to_owned(),
            ),
        ) {
            cleanup.push(observation);
        }
        cleanup.extend(
            join_extracted_local_composition_owner(
                state,
                resources,
                false,
                Some(self.local_composition_shutdown_telemetry_for_turn(turn_id)),
            )
            .await,
        );
        match cleanup.split_first() {
            Some((primary, remaining)) => {
                Err(error_with_cleanup_disposition(primary.clone(), remaining))
            }
            None => Ok(()),
        }
    }

    pub(super) fn handle_local_composition_broker_event(
        &mut self,
        state: &mut CompositionOwner,
        event: Option<LocalCompositionEvent>,
    ) -> Result<(), AgentError> {
        let Some(event) = event else {
            return Err(local_composition_started_error(
                "local composition broker closed while a cell was active",
            ));
        };
        match event {
            LocalCompositionEvent::Notification {
                notification,
                responder,
            } => {
                // Nothing a notification can be wrong about has an effect, so
                // every definite refusal is answered to the notifier and the
                // turn carries on. Only an error the driver itself cannot
                // decide still propagates.
                let (call_id, correlation) = match state
                    .notification_correlation(&notification.cell_id, &notification.call_id)
                {
                    Ok(correlation) => correlation,
                    Err(error) if local_composition_recovery_authoritative(&error) => {
                        return Err(error);
                    }
                    Err(error) => {
                        responder.reject(error.message().to_owned());
                        return Ok(());
                    }
                };
                let text = clip_local_composition_update_text(
                    &notification.text,
                    &mut self.observation_budget,
                )?;
                let Ok(update) = ToolResultUpdateBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    text,
                    correlation,
                ) else {
                    responder.reject("local composition notification is not canonical".to_owned());
                    return Ok(());
                };
                state.push_update(update);
                if !responder.acknowledge() {
                    return Err(local_composition_started_error(
                        "local composition notification acknowledgement receiver disappeared",
                    ));
                }
            }
            LocalCompositionEvent::NestedCall { call, responder } => {
                state.admit_nested(call, responder)?;
            }
        }
        Ok(())
    }
}
