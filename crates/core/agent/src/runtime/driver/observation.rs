//! Runtime telemetry context and emission.

use crate::runtime::composition::LocalCompositionShutdownTelemetry;
use crate::runtime::driver::Driver;
use crate::{TelemetryContext, TelemetryObservation, TelemetryObservationKind};
use plakous_model::TurnId;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
impl Driver {
    pub(super) fn emit_telemetry(&self, context: TelemetryContext, kind: TelemetryObservationKind) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.telemetry
                .emit(TelemetryObservation::new(context, self.clock.now(), kind));
        }));
    }

    pub(super) fn local_composition_shutdown_telemetry_for_turn(
        &self,
        turn_id: TurnId,
    ) -> LocalCompositionShutdownTelemetry {
        LocalCompositionShutdownTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context.with_turn(turn_id),
        }
    }

    pub(super) const fn telemetry_context_for_turn(
        &self,
        turn_id: Option<TurnId>,
    ) -> TelemetryContext {
        match turn_id {
            Some(turn_id) => self.telemetry_context.with_turn(turn_id),
            None => self.telemetry_context,
        }
    }
}
