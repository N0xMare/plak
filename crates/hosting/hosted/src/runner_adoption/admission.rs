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
    retry: Mutex<PendingRetry>,
}

#[derive(Default)]
struct PendingRetry {
    callback: Option<Retry>,
    opened: bool,
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
    opened: bool,
}

impl ReconciliationPermit {
    pub(crate) fn opened(&mut self) {
        self.opened = true;
    }
}

impl Drop for ReconciliationPermit {
    fn drop(&mut self) {
        let mut pending = self
            .gate
            .retry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.opened |= self.opened;
        drop(self.held.take());
        let retry = self.gate.retry_if_unheld(&mut pending);
        drop(pending);
        if let Some(retry) = retry {
            retry();
        }
    }
}

impl Gate {
    fn retry_if_unheld(&self, pending: &mut PendingRetry) -> Option<Retry> {
        let guard = self.lock.try_write().ok()?;
        let retry = pending.callback.take().filter(|_| !pending.opened);
        pending.opened = false;
        // Registration cannot run between taking the retry and releasing this guard.
        drop(guard);
        retry
    }

    fn try_lease_only(self: &Arc<Self>, retry: Option<Retry>) -> Option<ReconciliationPermit> {
        let mut pending = self.retry.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(guard) = self.lock.clone().try_write_owned() else {
            // Serialize the failed acquisition and retry registration with permit release.
            if pending.callback.is_none() {
                pending.callback = retry;
            }
            return None;
        };
        Some(ReconciliationPermit {
            held: Some(Held::LeaseOnly { _guard: guard }),
            gate: self.clone(),
            opened: false,
        })
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
        retry: Mutex::default(),
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
    Ok(gate(roots, line)?.try_lease_only(None))
}

pub(super) fn try_readoption(
    roots: &ProductStateRoots,
    line: LineId,
) -> CliResult<Option<ReconciliationPermit>> {
    Ok(gate(roots, line)?.try_lease_only(roots.runner_reconciliation_retry()))
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
        opened: false,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn count_retry(retries: &Arc<AtomicUsize>) -> Option<Retry> {
        let recorded = retries.clone();
        Some(Box::new(move || {
            recorded.fetch_add(1, Ordering::SeqCst);
        }))
    }

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
        let directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let gate = gate(&roots, line).unwrap();
        let first = wait_for_gate(gate.clone()).await.unwrap();
        let second = wait_for_gate(gate.clone()).await.unwrap();
        let retries = Arc::new(AtomicUsize::new(0));
        assert!(gate.try_lease_only(count_retry(&retries)).is_none());
        drop(first);
        assert_eq!(retries.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(retries.load(Ordering::SeqCst), 1);
        assert!(try_lease_only(&roots, line).unwrap().is_some());
        assert_eq!(retries.load(Ordering::SeqCst), 1);
        assert!(gate.retry.lock().unwrap().callback.is_none());
    }

    #[tokio::test]
    async fn a_successful_open_covers_skips_until_the_last_overlapping_open_releases() {
        let directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let gate = gate(&roots, line).unwrap();
        let mut opened = wait_for_gate(gate.clone()).await.unwrap();
        let failed = wait_for_gate(gate.clone()).await.unwrap();
        let retries = Arc::new(AtomicUsize::new(0));
        assert!(gate.try_lease_only(count_retry(&retries)).is_none());
        opened.opened();
        drop(opened);
        assert!(gate.try_lease_only(count_retry(&retries)).is_none());
        drop(failed);
        assert_eq!(retries.load(Ordering::SeqCst), 0);
        assert!(gate.retry.lock().unwrap().callback.is_none());
        let failed = wait_for_gate(gate.clone()).await.unwrap();
        assert!(gate.try_lease_only(count_retry(&retries)).is_none());
        drop(failed);
        assert_eq!(retries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_skip_racing_the_last_failed_open_never_loses_its_retry() {
        let directory = tempfile::tempdir().unwrap();
        let roots = fixture_roots(directory.path());
        let line = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let gate = gate(&roots, line).unwrap();
        let retries = Arc::new(AtomicUsize::new(0));
        let mut skipped = 0;
        for _ in 0..200 {
            let opening = wait_for_gate(gate.clone()).await.unwrap();
            let start = Arc::new(std::sync::Barrier::new(2));
            let worker = {
                let gate = gate.clone();
                let start = start.clone();
                let retries = retries.clone();
                std::thread::spawn(move || {
                    start.wait();
                    gate.try_lease_only(count_retry(&retries)).is_none()
                })
            };
            start.wait();
            drop(opening);
            skipped += usize::from(worker.join().unwrap());
            assert_eq!(retries.load(Ordering::SeqCst), skipped);
            assert!(gate.retry.lock().unwrap().callback.is_none());
        }
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
