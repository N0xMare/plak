//! Reload probes available to both CLI queries and interactive handoff.

use super::document::IMPORTABLE_VERSIONS;
use crate::home::paths::PlakPaths;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCapacityProbe {
    pub(crate) route: String,
    pub(crate) total_window: Option<u64>,
    pub(crate) admitted_input: Option<u64>,
    pub(crate) estimated_tokens: u64,
    pub(crate) messages: usize,
}

impl CarryCapacityProbe {
    pub(crate) fn warning(&self) -> Option<String> {
        (self.admitted_input.is_some_and(|input|self.estimated_tokens > input)).then(|| format!(
            "estimated history ~{} tokens exceeds destination route {} input window {} tokens (total window {}); the first prompt will refuse unless the history compacts",
            self.estimated_tokens,self.route,self.admitted_input.unwrap_or(0),
            self.total_window.map_or_else(|| "not reported".to_owned(), |n|format!("{n} tokens")),
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ReloadProbe {
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) config_error: Option<String>,
}

pub(crate) fn reload_probe(paths: &PlakPaths) -> ReloadProbe {
    ReloadProbe {
        state_key: crate::home::state::state_key(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: crate::product_config::SessionPolicy::load(paths).err(),
    }
}
