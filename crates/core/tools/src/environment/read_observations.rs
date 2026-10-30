use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use plakous_model::RunId;

use super::{ExecutionEnvironment, WorkspaceContentIdentity};

// Losing an observation only requires a reread; it must never grant a write.
const MAX_READ_OBSERVATIONS: usize = 1_024;

#[derive(Default)]
pub(super) struct ReadObservations(Mutex<Observations>);

#[derive(Default)]
struct Observations {
    identities: BTreeMap<(RunId, PathBuf), WorkspaceContentIdentity>,
    order: VecDeque<(RunId, PathBuf)>,
}

impl ExecutionEnvironment {
    fn read_observation_key(&self, run: RunId, path: &Path) -> Option<(RunId, PathBuf)> {
        if run != self.descriptor().scope().run_id() {
            return None;
        }
        // This does not open a file for a tool or relax the backend's path checks.
        self.workspace()
            .resolve_existing(path)
            .ok()
            .map(|path| (run, path))
    }

    /// Records the whole-file identity after an explicit text read succeeds.
    /// Clones share these bounded, ephemeral observations; reconstructed
    /// environments start empty. Implicit backend reads must not call this.
    pub fn record_text_read(&self, run: RunId, path: &Path, identity: WorkspaceContentIdentity) {
        let Some(key) = self.read_observation_key(run, path) else {
            return;
        };
        let Ok(mut observations) = self.read_observations.0.lock() else {
            return;
        };
        observations.order.retain(|entry| entry != &key);
        observations.order.push_back(key.clone());
        observations.identities.insert(key, identity);
        while observations.order.len() > MAX_READ_OBSERVATIONS {
            if let Some(evicted) = observations.order.pop_front() {
                observations.identities.remove(&evicted);
            }
        }
    }

    /// Returns this attachment's last explicit text-read identity, if retained.
    #[must_use]
    pub fn last_text_read(&self, run: RunId, path: &Path) -> Option<WorkspaceContentIdentity> {
        let key = self.read_observation_key(run, path)?;
        self.read_observations
            .0
            .lock()
            .ok()?
            .identities
            .get(&key)
            .copied()
    }

    /// Invalidates observations for a workspace-relative mutation target.
    /// Call before publication, including deletes, so aliases still resolve.
    pub(crate) fn invalidate_text_read(&self, run: RunId, path: &Path) {
        let Some(key) = self.read_observation_key(run, path) else {
            return;
        };
        let Ok(mut observations) = self.read_observations.0.lock() else {
            return;
        };
        observations.identities.remove(&key);
        observations.order.retain(|entry| entry != &key);
    }
}

#[cfg(test)]
mod tests;
