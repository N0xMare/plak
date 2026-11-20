use std::sync::Arc;

use super::{ProductTelemetry, TelemetryHealth, TelemetrySinkFactory};
use super::{RosterSource, TreeStoreFactory};
use crate::CliResult;
use crate::agent_tree::{MemoryTreeStore, TreeStore};

/// The conformance-tested in-memory tree store, opened once per factory.
#[derive(Debug)]
pub struct MemoryTreeStoreFactory;

impl TreeStoreFactory for MemoryTreeStoreFactory {
    fn open(&self) -> CliResult<Arc<dyn TreeStore>> {
        Ok(Arc::new(MemoryTreeStore::default()))
    }
}

/// A roster that freezes the tuple it is handed and reads no configuration.
#[derive(Debug)]
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
        root_account.clone_into(&mut input.root.account);
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

/// No evaluator workers are installed.
#[derive(Debug)]
pub struct NoSidecars;

impl plakous::tools::SidecarLocator for NoSidecars {
    fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no JavaScript evaluator worker is installed in this test".to_owned())
    }

    fn python_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no Python evaluator worker is installed in this test".to_owned())
    }
}

/// No `OpenRouter` key is configured.
#[derive(Debug)]
pub struct NoOpenRouterCredential;

#[cfg(feature = "openrouter")]
mod openrouter;
#[cfg(not(feature = "openrouter"))]
#[path = "doubles/openrouter_unavailable.rs"]
mod openrouter;

pub(crate) struct NoTelemetry;

impl TelemetrySinkFactory for NoTelemetry {
    fn open(&self) -> Arc<dyn ProductTelemetry> {
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
