//! Runtime telemetry context and emission.

use crate::runtime::compaction::request::total_context_window_tokens;
use crate::runtime::composition::LocalCompositionShutdownTelemetry;
use crate::runtime::driver::Driver;
use crate::{
    CompactionMeasurement, ModelMeasurement, TelemetryContext, TelemetryObservation,
    TelemetryObservationKind,
};
use plakous_model::{ContextStatus, StableRequestPrefix, TurnId};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

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

    fn route_context_window(&self) -> Option<u64> {
        self.provider_configuration
            .effective()
            .descriptor()
            .context_contract()
            .and_then(|contract| total_context_window_tokens(contract.capacity().rule()))
    }

    /// One model attempt's measurement. `first_attempt` adds the stable
    /// prefix footprint measured once at spawn.
    pub(super) fn model_measurement(
        &self,
        status: Option<&ContextStatus>,
        first_attempt: bool,
        idle_gap: Option<Duration>,
    ) -> ModelMeasurement {
        if !self.telemetry.is_enabled() {
            return ModelMeasurement::absent();
        }
        ModelMeasurement::new(
            self.route_context_window(),
            prepared_input_tokens(status),
            self.request_footprint.filter(|_| first_attempt),
            idle_gap,
        )
    }

    /// The window, and the trigger and occupancy of the status that decided
    /// a compaction. A compaction no status decided passes `None`.
    pub(super) fn compaction_measurement(
        &self,
        status: Option<&ContextStatus>,
    ) -> CompactionMeasurement {
        if !self.telemetry.is_enabled() {
            return CompactionMeasurement::absent();
        }
        CompactionMeasurement::new(
            self.route_context_window(),
            status.and_then(ContextStatus::compaction_trigger_tokens),
            status.and_then(ContextStatus::occupancy_tokens),
            None,
        )
    }

    /// Time since this driver's previous provider request; marks this one.
    /// Called once per model attempt or paid compaction call, before any
    /// retry.
    pub(super) fn note_provider_request(&mut self) -> Option<Duration> {
        if !self.telemetry.is_enabled() {
            return None;
        }
        let now = tokio::time::Instant::now();
        self.last_provider_request_at
            .replace(now)
            .map(|then| now.saturating_duration_since(then))
    }
}

/// UTF-8 instruction bytes and serialized tool input schema bytes of a stable
/// request prefix.
pub(super) fn request_footprint(prefix: &StableRequestPrefix) -> (u64, u64) {
    let instructions = prefix.instructions().iter().fold(0_u64, |total, block| {
        total.saturating_add(u64::try_from(block.text().len()).unwrap_or(u64::MAX))
    });
    let tools = prefix.tools().iter().fold(0_u64, |total, tool| {
        let bytes =
            serde_json::to_vec(tool.definition().input()).map_or(0, |encoded| encoded.len());
        total.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX))
    });
    (instructions, tools)
}

fn prepared_input_tokens(status: Option<&ContextStatus>) -> Option<u64> {
    status.and_then(|status| {
        status.local_input_upper_bound().or_else(|| {
            status
                .measurement()
                .map(|measurement| measurement.bound().upper_bound())
        })
    })
}
