//! Process-local ordering between runtime opens and lease-only readoption.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use plakous::model::LineId;
use plakous::sessions::SessionLocator;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

type Gates = BTreeMap<(PathBuf, LineId), Weak<RwLock<()>>>;
static GATES: LazyLock<Mutex<Gates>> = LazyLock::new(Mutex::default);

fn gate(database: &Path, line: LineId) -> Arc<RwLock<()>> {
    let mut gates = GATES.lock().unwrap_or_else(PoisonError::into_inner);
    gates.retain(|_, gate| gate.strong_count() != 0);
    let key = (
        std::fs::canonicalize(database).unwrap_or_else(|_| database.to_path_buf()),
        line,
    );
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(RwLock::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

pub(super) fn try_readoption(database: &Path, line: LineId) -> Option<OwnedRwLockWriteGuard<()>> {
    gate(database, line).try_write_owned().ok()
}

async fn wait_for_readoption(database: &Path, line: LineId) -> Option<OwnedRwLockReadGuard<()>> {
    let gate = gate(database, line);
    if let Ok(guard) = gate.clone().try_read_owned() {
        return Some(guard);
    }
    crate::test_support_runner::readoption_waiting();
    tokio::time::timeout(Duration::from_secs(2), gate.read_owned())
        .await
        .ok()
}

pub(crate) async fn admit_open(
    roots: &crate::agent::ProductStateRoots,
    locator: SessionLocator,
) -> Result<OwnedRwLockReadGuard<()>, crate::agent::CodingSessionOpenFailure> {
    let line = roots
        .runtime_sessions()?
        .line_head_of(locator)
        .await?
        .line();
    let Some(guard) = wait_for_readoption(roots.runtime_database(), line).await else {
        let mut failure =
            crate::agent::CodingSessionOpenFailure::from(crate::CliFailure::recovery_required(
                "this host is still reconciling the session's runner; retry shortly",
            ));
        failure.lease_held = true;
        return Err(failure);
    };
    Ok(guard)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "process-local admission fixtures")]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn readoption_waits_only_within_its_database_and_remains_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("runtime.sqlite3");
        let other = directory.path().join("other.sqlite3");
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let held = try_readoption(&database, line).unwrap();
        let foreign = wait_for_readoption(&other, line).await.unwrap();
        assert!(wait_for_readoption(&database, line).await.is_none());
        drop(held);
        let opening = wait_for_readoption(&database, line).await.unwrap();
        assert!(
            try_readoption(&database, line).is_none(),
            "a sweep must skip a runtime opening"
        );
        drop(opening);
        assert!(try_readoption(&database, line).is_some());
        drop(foreign);
    }
}
