//! No output or store is opened for detached inspection and semantic compilation.
use super::{ExtraTelemetrySink, ProductTelemetry, TelemetryHealth, TelemetrySinkFactory};
use std::sync::Arc;

pub(crate) struct NoTelemetry;

impl TelemetrySinkFactory for NoTelemetry {
    fn open(&self, _extra_sink: Option<Arc<dyn ExtraTelemetrySink>>) -> Arc<dyn ProductTelemetry> {
        Arc::new(Self)
    }
    fn log_unavailable(&self, _session_id: &str, _reason: &str) {}
}

impl ProductTelemetry for NoTelemetry {
    fn agent_emitter(&self) -> Option<Arc<plakous::agent::TelemetryEmitter>> {
        None
    }
    fn topology_observer(&self) -> Option<Arc<dyn crate::agent_tree::TopologyObserver>> {
        None
    }
    fn emit(&self, _observation: crate::product_observation::ProductObservation) {}
    fn health(&self) -> TelemetryHealth {
        TelemetryHealth::Missing
    }
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
}
