//! Shared runner custody exists only in host-capable products and fixtures.

use super::{CliFailure, CliResult, PlakPaths, RunnerOwnership, TryLockError, lock_file};

impl RunnerOwnership {
    pub(crate) fn host(paths: &PlakPaths) -> CliResult<Self> {
        let file = lock_file(paths)?;
        file.try_lock_shared().map_err(|error| match error {
            TryLockError::WouldBlock => CliFailure::new(
                "a standalone runner operation owns this home; retry after it finishes",
            ),
            TryLockError::Error(error) => {
                CliFailure::new(format!("runner ownership lock: {error}"))
            }
        })?;
        Ok(Self {
            standalone: false,
            _lock: file,
        })
    }
}
