//! Coalesced probe ownership is independent of any picker request.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::{Connection, PlacementFailure, ProbePhase, RunnerRegistry, RunnerRow};

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "deterministic probe fixtures")]
mod tests;

/// One separately owned transport/options/reconciliation probe.
pub type RunnerProbeFuture = Pin<Box<dyn Future<Output = Result<(), PlacementFailure>> + Send>>;
/// Host composition supplies strict dialing and first-bind recovery; the
/// registry supplies coalescing, admission, budgets and request cancellation.
pub type RunnerProbe = dyn Fn(RunnerRegistry, String) -> RunnerProbeFuture + Send + Sync;

pub(super) struct ProbeState {
    pub(super) active: bool,
    pub(super) reconcile_again: bool,
    admitted: Option<Instant>,
    completed: Option<Instant>,
    pub(super) phase: ProbePhase,
}

impl Default for ProbeState {
    fn default() -> Self {
        Self {
            active: false,
            reconcile_again: false,
            admitted: None,
            completed: None,
            phase: ProbePhase::Queued,
        }
    }
}

impl ProbeState {
    pub(super) fn invalidate_cache(&mut self) {
        self.completed = None;
    }
}

impl RunnerRegistry {
    /// Installs the process-owned probe, not a request-owned dialer. It must
    /// publish ready only after sweep, recover, recorded stops and cleanup.
    pub fn probe_with(&self, probe: Arc<RunnerProbe>) {
        *self.probe.write().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    }

    /// Refreshes selected configured identities (all when absent). Probes
    /// coalesce, cache completed results for five seconds, admit at most four
    /// concurrently and get 25 seconds after pool admission. This future owns
    /// only its reads: dropping it cannot cancel queued probes or recovery.
    ///
    /// # Errors
    /// Unknown identities refuse before any selected probe is queued.
    pub async fn refresh(
        &self,
        ids: Option<&[String]>,
    ) -> Result<Vec<RunnerRow>, PlacementFailure> {
        self.refresh_configuration()?;
        let ids = {
            let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
            let ids = ids.map_or_else(
                || {
                    entries
                        .values()
                        .filter(|entry| !entry.row.runner.names.is_empty())
                        .map(|entry| entry.row.runner.runner_id.clone())
                        .collect::<Vec<_>>()
                },
                <[String]>::to_vec,
            );
            for id in &ids {
                if entries
                    .get(id)
                    .is_none_or(|entry| entry.row.runner.names.is_empty())
                {
                    return Err(super::missing(id));
                }
            }
            ids
        };
        let mut changed = self.changes();
        for id in &ids {
            self.queue_probe(id);
        }
        loop {
            let mut remaining = None;
            let mut waiting = false;
            {
                let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
                for id in &ids {
                    let Some(entry) = entries.get(id) else {
                        continue;
                    };
                    if !entry.refresh.active {
                        continue;
                    }
                    if let Some(admitted) = entry.refresh.admitted {
                        let budget = Duration::from_secs(25).saturating_sub(admitted.elapsed());
                        if budget.is_zero() {
                            continue;
                        }
                        remaining =
                            Some(remaining.map_or(budget, |prior: Duration| prior.min(budget)));
                    }
                    waiting = true;
                }
            }
            if !waiting {
                return Ok(self.request_rows(&ids));
            }
            if let Some(remaining) = remaining {
                tokio::select! { _ = changed.changed() => {}, () = tokio::time::sleep(remaining) => {} }
            } else {
                let _ = changed.changed().await;
            }
        }
    }

    // Readers project their own exhausted budget; they never overwrite a
    // probe's later completed row or take ownership of its reconciliation.
    fn request_rows(&self, ids: &[String]) -> Vec<RunnerRow> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|entry| {
                ids.contains(&entry.row.runner.runner_id) && !entry.row.runner.names.is_empty()
            })
            .map(|entry| {
                let mut row = entry.row.clone();
                if entry.refresh.active
                    && entry
                        .refresh
                        .admitted
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(25))
                {
                    row.status =
                        super::RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
                            phase: entry.refresh.phase,
                            timed_out: true,
                        });
                    row.blocked_since.get_or_insert_with(now);
                    row.checked_at = Some(now());
                }
                row
            })
            .collect()
    }

    pub(super) fn queue_probe(&self, id: &str) {
        {
            let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            let needed = entry.required
                || match &entry.connection {
                    Some(Connection::Remote(client, services)) => {
                        client.has_pending_calls() || services.has_node_users()
                    }
                    _ => false,
                };
            let failed = matches!(entry.row.status, super::RunnerStatus::Blocked(_));
            let disconnected = match &entry.connection {
                Some(Connection::Remote(client, _) | Connection::Local(_, Some(client))) => {
                    !client.is_connected()
                }
                _ => false,
            };
            if entry.refresh.active
                || (entry
                    .refresh
                    .completed
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
                    && !(needed && failed)
                    && !disconnected)
            {
                return;
            }
            entry.refresh.active = true;
            entry.refresh.admitted = None;
        }
        let registry = self.clone();
        let id = id.to_owned();
        self.progress(&id, ProbePhase::Queued);
        let work = async move {
            registry.run_probe(id).await;
        };
        if let Some(executor) = self
            .executor
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            executor.spawn(work);
        } else {
            tokio::spawn(work);
        }
    }

    async fn run_probe(&self, id: String) {
        let Ok(permit) = self.probe_pool.clone().acquire_owned().await else {
            self.finish_probe(&id);
            return;
        };
        loop {
            if let Some(entry) = self
                .entries
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(&id)
            {
                entry.refresh.admitted = Some(Instant::now());
            }
            self.changed
                .send_modify(|value| *value = value.wrapping_add(1));
            let probe = self
                .probe
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let registry = self.clone();
            let selected = id.clone();
            let mut work = tokio::spawn(async move {
                match probe {
                    Some(probe) => probe(registry, selected).await,
                    None => registry.refresh_connected(&selected).await,
                }
            });
            let result = tokio::select! {
                result = &mut work => result,
                () = tokio::time::sleep(Duration::from_secs(25)) => {
                    let phase = self.entries.read().unwrap_or_else(PoisonError::into_inner).get(&id).map_or(ProbePhase::Reconcile, |entry| entry.refresh.phase);
                    self.blocked(&id, PlacementFailure::RunnerUnreachable {phase, timed_out: true}, now());
                    work.await
                }
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => self.blocked(&id, failure, now()),
                Err(_) => self.blocked(
                    &id,
                    PlacementFailure::RunnerUnreachable {
                        phase: ProbePhase::Reconcile,
                        timed_out: false,
                    },
                    now(),
                ),
            }
            if !self.finish_probe(&id) {
                break;
            }
        }
        drop(permit);
    }

    fn finish_probe(&self, id: &str) -> bool {
        let mut again = false;
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            again = std::mem::take(&mut entry.refresh.reconcile_again);
            entry.refresh.active = again;
            entry.refresh.completed = (!again).then(Instant::now);
            entry.refresh.admitted = None;
        }
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
        again
    }

    async fn refresh_connected(&self, id: &str) -> Result<(), PlacementFailure> {
        match self.connection(id)? {
            Connection::Local(catalog, client) => match client {
                Some(client) => {
                    self.ready_local_daemon(catalog, client.connection_revision(), now())?;
                }
                None => self.ready_local(catalog, now())?,
            },
            Connection::Remote(client, services) => {
                let connection = client.connection_revision();
                self.progress(id, ProbePhase::Options);
                let options = client
                    .workspace_options()
                    .await
                    .map_err(|_| unavailable(ProbePhase::Options))?;
                self.progress(id, ProbePhase::Discovery);
                let discovery = client
                    .workspace_discover()
                    .await
                    .map_err(|_| unavailable(ProbePhase::Discovery))?;
                self.ready(id, connection, services, options, discovery, now())?;
            }
        }
        Ok(())
    }
}

fn unavailable(phase: ProbePhase) -> PlacementFailure {
    PlacementFailure::RunnerUnreachable {
        phase,
        timed_out: false,
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}
