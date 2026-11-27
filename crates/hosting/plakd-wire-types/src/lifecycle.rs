//! Planned host transitions are advisory, not prompt acceptance or replay authority.

use serde::{Deserialize, Serialize};

pub type HostLifecycle = plakous_host_protocol::wire::HostLifecycle<crate::PlakExtension>;
pub use plakous_host_protocol::wire::BridgeUnavailableReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostLifecyclePhase {
    Starting,
    Draining,
    Restarting,
    Stopping,
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostLifecycleReason {
    Startup,
    PairedUpgrade,
    CrossKeyUpgrade,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostLifecycleTarget {
    pub binary_id: String,
    pub state_key: String,
}
