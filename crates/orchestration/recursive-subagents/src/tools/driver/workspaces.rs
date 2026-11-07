//! Workspace-size caching and explicit runner probes.

use super::{Arc, TreeDriver};

pub(super) struct CheckoutSize {
    generation: u64,
    trim_epoch: u64,
    path: std::path::PathBuf,
    measured_at: u64,
    bytes: u64,
}

impl CheckoutSize {
    fn reusable(
        &self,
        path: &std::path::Path,
        generation: u64,
        now: u64,
        retry_at: u64,
        trim_epoch: u64,
    ) -> bool {
        self.path == path
            && self.generation == generation
            && self.trim_epoch == trim_epoch
            && (now < retry_at || now.saturating_sub(self.measured_at) < 30_000)
    }
}

pub(super) fn probe_checkout_sizes(
    cache: &mut std::collections::BTreeMap<String, CheckoutSize>,
    inventory: Vec<(String, std::path::PathBuf, u64, u64, u64)>,
    now: u64,
    mut walk: impl FnMut(&std::path::Path) -> Result<Option<u64>, plakous::tools::ProbeError>,
) -> Result<std::collections::BTreeMap<String, u64>, plakous::tools::ProbeError> {
    cache.retain(|id, _| inventory.iter().any(|(current, ..)| current == id));
    let mut sizes = std::collections::BTreeMap::new();
    for (id, path, generation, retry_at, trim_epoch) in inventory {
        if !cache
            .get(&id)
            .is_some_and(|size| size.reusable(&path, generation, now, retry_at, trim_epoch))
        {
            // An unmeasured copy is left out; a probe that cannot answer
            // ends the pass rather than reclaim on partial sizes.
            let Some(bytes) = walk(&path)? else {
                continue;
            };
            cache.insert(
                id.clone(),
                CheckoutSize {
                    generation,
                    trim_epoch,
                    path,
                    measured_at: now,
                    bytes,
                },
            );
        }
        if let Some(size) = cache.get(&id) {
            sizes.insert(id, size.bytes);
        }
    }
    Ok(sizes)
}

impl TreeDriver {
    /// Installs the explicit reader used by periodic workspace-size probes.
    pub fn set_workspace_probe(&self, probe: Arc<dyn plakous::tools::WorkspaceProbe>) {
        *self
            .workspace_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = probe;
    }
}
