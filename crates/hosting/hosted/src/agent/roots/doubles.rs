use std::sync::Arc;

use super::{
    ProductTelemetry, RosterSource, TelemetryHealth, TelemetrySinkFactory, TreeStoreFactory,
};
use crate::CliResult;
use crate::agent_tree::{MemoryTreeStore, TopologyObserver, TreeStore};

/// The conformance-tested in-memory tree store, opened once per factory.
pub struct MemoryTreeStoreFactory;

impl TreeStoreFactory for MemoryTreeStoreFactory {
    fn open(&self) -> CliResult<Arc<dyn TreeStore>> {
        Ok(Arc::new(MemoryTreeStore::default()))
    }
}

/// A roster that freezes the tuple it is handed and reads no configuration.
pub struct UnpinnedRosterSource;

impl RosterSource for UnpinnedRosterSource {
    fn roster(
        &self,
        root_account: &str,
        root_tuple: &str,
        root_effort: Option<&str>,
        _root_profile: Option<&crate::sessions::RootProfile>,
    ) -> CliResult<crate::orchestration::RosterInput> {
        let mut input = crate::orchestration::RosterInput::unpinned(root_tuple);
        input.root.account = root_account.to_owned();
        input.root.effort = root_effort.map(str::to_owned);
        Ok(input)
    }

    fn account_name(&self, provider: plakous::subscriptions::NativeProvider) -> &'static str {
        match provider {
            plakous::subscriptions::NativeProvider::Codex => "codex",
            plakous::subscriptions::NativeProvider::Claude => "claude",
            _ => "grok",
        }
    }

    fn topology_admitted(&self) -> CliResult<bool> {
        Ok(true)
    }
}

/// Telemetry that records nothing and is never unavailable.
pub struct NoTelemetry;

impl TelemetrySinkFactory for NoTelemetry {
    fn open(
        &self,
        _extra_sink: Option<Arc<dyn super::ExtraTelemetrySink>>,
    ) -> Arc<dyn ProductTelemetry> {
        Arc::new(Self)
    }

    fn log_unavailable(&self, _session_id: &str, _reason: &str) {}
}

impl ProductTelemetry for NoTelemetry {
    fn agent_emitter(&self) -> Option<Arc<plakous::agent::TelemetryEmitter>> {
        None
    }

    fn topology_observer(&self) -> Option<Arc<dyn TopologyObserver>> {
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

/// No evaluator workers are installed.
pub struct NoSidecars;

impl plakous::tools::SidecarLocator for NoSidecars {
    fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no JavaScript evaluator worker is installed in this test".to_owned())
    }

    fn python_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no Python evaluator worker is installed in this test".to_owned())
    }
}

/// No OpenRouter key is configured.
pub struct NoOpenRouterCredential;

#[cfg(feature = "openrouter")]
mod openrouter;
#[cfg(not(feature = "openrouter"))]
#[path = "doubles/openrouter_unavailable.rs"]
mod openrouter;
