//! Process-local ordering of root opens and their lease-only reconciliation.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use plakous::model::LineId;
use plakous::sessions::SessionLocator;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::agent::{CodingSessionOpenFailure, ProductStateRoots};
use crate::{CliFailure, CliResult};

type Retry = Box<dyn FnOnce() + Send>;
type Gates = BTreeMap<(u64, u64, LineId), Weak<Gate>>;
static GATES: LazyLock<Mutex<Gates>> = LazyLock::new(Mutex::default);

struct Gate {
    lock: Arc<RwLock<()>>,
    retry: Mutex<Option<Retry>>,
}

enum Held {
    Open { _guard: OwnedRwLockReadGuard<()> },
    LeaseOnly { _guard: OwnedRwLockWriteGuard<()> },
}

/// An in-process scheduling permit, never a store lease or runner authority.
#[must_use]
pub struct ReconciliationPermit {
    held: Option<Held>,
    gate: Arc<Gate>,
}

impl Drop for ReconciliationPermit {
    fn drop(&mut self) {
        drop(self.held.take());
        self.gate.retry_if_unheld();
    }
}

impl Gate {
    fn request_retry(&self, retry: Option<Retry>) {
        let mut retained = self.retry.lock().unwrap_or_else(PoisonError::into_inner);
        if retained.is_none() {
            *retained = retry;
        }
        drop(retained);
        // Release may have preceded registration; never leave that retry asleep.
        self.retry_if_unheld();
    }

    fn retry_if_unheld(&self) {
        let Ok(guard) = self.lock.try_write() else {
            return;
        };
        let retry = self
            .retry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(guard);
        if let Some(retry) = retry {
            retry();
        }
    }
}

fn gate(roots: &ProductStateRoots, line: LineId) -> CliResult<Arc<Gate>> {
    let metadata = std::fs::metadata(roots.runtime_database()).map_err(|error| {
        CliFailure::new(format!(
            "the runtime database identity could not be read: {error}"
        ))
    })?;
    let key = (metadata.dev(), metadata.ino(), line);
    let mut gates = GATES.lock().unwrap_or_else(PoisonError::into_inner);
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return Ok(gate);
    }
    let gate = Arc::new(Gate {
        lock: Arc::new(RwLock::new(())),
        retry: Mutex::new(None),
    });
    gates.insert(key, Arc::downgrade(&gate));
    Ok(gate)
}

/// Tries one line's scheduling permit before acquiring its ordinary store lease.
/// A busy local open or reconciliation is not a foreign owner and is never waited on.
///
/// # Errors
/// The runtime database identity could not be read.
pub fn try_lease_only(
    roots: &ProductStateRoots,
    line: LineId,
) -> CliResult<Option<ReconciliationPermit>> {
    let gate = gate(roots, line)?;
    Ok(gate
        .lock
        .clone()
        .try_write_owned()
        .ok()
        .map(|guard| ReconciliationPermit {
            held: Some(Held::LeaseOnly { _guard: guard }),
            gate,
        }))
}

pub(super) fn retry_after_open(roots: &ProductStateRoots, line: LineId) -> CliResult<()> {
    let gate = gate(roots, line)?;
    gate.request_retry(roots.runner_reconciliation_retry());
    Ok(())
}

async fn wait_for_gate(gate: Arc<Gate>) -> Option<ReconciliationPermit> {
    let guard = if let Ok(guard) = gate.lock.clone().try_read_owned() {
        guard
    } else {
        crate::test_support_runner::readoption_waiting();
        tokio::time::timeout(Duration::from_secs(2), gate.lock.clone().read_owned())
            .await
            .ok()?
    };
    Some(ReconciliationPermit {
        held: Some(Held::Open { _guard: guard }),
        gate,
    })
}

pub(crate) async fn admit_open(
    roots: &ProductStateRoots,
    locator: SessionLocator,
) -> Result<ReconciliationPermit, CodingSessionOpenFailure> {
    let line = roots
        .runtime_sessions()?
        .line_head_of(locator)
        .await?
        .line();
    wait_for_gate(gate(roots, line)?).await.ok_or_else(|| {
        CliFailure::recovery_required(
            "this host is still reconciling the session's runner; retry shortly",
        )
        .into()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "process-local admission fixtures")]
mod tests {
    use super::*;

    fn fixture_roots(directory: &std::path::Path) -> ProductStateRoots {
        let roots =
            ProductStateRoots::with_doubles(directory.to_path_buf(), directory.join("state"));
        std::fs::write(roots.runtime_database(), b"identity fixture").unwrap();
        roots
    }

    #[tokio::test(start_paused = true)]
    async fn lease_only_wait_is_database_scoped_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let other_directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let other = fixture_roots(other_directory.path());
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let held = try_lease_only(&roots, line).unwrap().unwrap();
        let foreign = wait_for_gate(gate(&other, line).unwrap()).await.unwrap();
        let started = tokio::time::Instant::now();
        assert!(wait_for_gate(gate(&roots, line).unwrap()).await.is_none());
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        drop(held);
        let opening = wait_for_gate(gate(&roots, line).unwrap()).await.unwrap();
        assert!(try_lease_only(&roots, line).unwrap().is_none());
        drop(opening);
        assert!(try_lease_only(&roots, line).unwrap().is_some());
        drop(foreign);
    }

    #[tokio::test]
    async fn the_last_open_release_retries_skipped_reconciliation_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let gate = gate(&roots, line).unwrap();
        let first = wait_for_gate(gate.clone()).await.unwrap();
        let second = wait_for_gate(gate.clone()).await.unwrap();
        let retries = Arc::new(AtomicUsize::new(0));
        let recorded = retries.clone();
        gate.request_retry(Some(Box::new(move || {
            recorded.fetch_add(1, Ordering::SeqCst);
        })));
        drop(first);
        assert_eq!(retries.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(retries.load(Ordering::SeqCst), 1);
        assert!(try_lease_only(&roots, line).unwrap().is_some());
        assert_eq!(retries.load(Ordering::SeqCst), 1);
        let recorded = retries.clone();
        gate.request_retry(Some(Box::new(move || {
            recorded.fetch_add(1, Ordering::SeqCst);
        })));
        assert_eq!(
            retries.load(Ordering::SeqCst),
            2,
            "registration after release must not lose the retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn database_aliases_share_the_gate_and_missing_identity_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let alias_directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let alias = ProductStateRoots::with_doubles(
            alias_directory.path().to_path_buf(),
            alias_directory.path().join("state"),
        );
        std::fs::hard_link(roots.runtime_database(), alias.runtime_database()).unwrap();
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let _held = try_lease_only(&roots, line).unwrap().unwrap();
        assert!(wait_for_gate(gate(&alias, line).unwrap()).await.is_none());
        std::fs::remove_file(alias.runtime_database()).unwrap();
        assert!(try_lease_only(&alias, line).is_err());
    }
}
