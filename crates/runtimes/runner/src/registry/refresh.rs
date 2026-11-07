//! Coalesced probe ownership is independent of any picker request.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::{Connection, PlacementFailure, ProbePhase, RunnerRegistry, RunnerRow};

#[cfg(test)]
mod tests;

/// One separately owned transport/options/reconciliation probe.
pub type RunnerProbeFuture = Pin<Box<dyn Future<Output = Result<(), PlacementFailure>> + Send>>;
/// Host composition supplies strict dialing and first-bind recovery; the
/// registry supplies coalescing, admission, budgets and request cancellation.
pub type RunnerProbe = dyn Fn(RunnerRegistry, String) -> RunnerProbeFuture + Send + Sync;

pub(super) struct ProbeState {
    active: bool,
    admitted: Option<Instant>,
    completed: Option<Instant>,
    pub(super) phase: ProbePhase,
}

impl Default for ProbeState {
    fn default() -> Self {
        Self {
            active: false,
            admitted: None,
            completed: None,
            phase: ProbePhase::Queued,
        }
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
                if !entries
                    .get(id)
                    .is_some_and(|entry| !entry.row.runner.names.is_empty())
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
            let mut expired = Vec::new();
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
                            if !matches!(
                                entry.row.status,
                                super::RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
                                    timed_out: true,
                                    ..
                                })
                            ) {
                                expired.push((id.clone(), entry.refresh.phase));
                            }
                            continue;
                        }
                        remaining =
                            Some(remaining.map_or(budget, |prior: Duration| prior.min(budget)));
                    }
                    waiting = true;
                }
            }
            for (id, phase) in expired {
                self.blocked(
                    &id,
                    PlacementFailure::RunnerUnreachable {
                        phase,
                        timed_out: true,
                    },
                    now(),
                );
            }
            if !waiting {
                return Ok(self
                    .rows()
                    .into_iter()
                    .filter(|row| ids.contains(&row.runner.runner_id))
                    .collect());
            }
            if let Some(remaining) = remaining {
                tokio::select! { _ = changed.changed() => {}, () = tokio::time::sleep(remaining) => {} }
            } else {
                let _ = changed.changed().await;
            }
        }
    }

    fn queue_probe(&self, id: &str) {
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
            if entry.refresh.active
                || (entry
                    .refresh
                    .completed
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
                    && !(needed && failed))
            {
                return;
            }
            entry.refresh.active = true;
            entry.refresh.admitted = None;
        }
        let registry = self.clone();
        let id = id.to_owned();
        self.progress(&id, ProbePhase::Queued);
        tokio::spawn(async move {
            registry.run_probe(id).await;
        });
    }

    async fn run_probe(&self, id: String) {
        let Ok(permit) = self.probe_pool.clone().acquire_owned().await else {
            self.finish_probe(&id);
            return;
        };
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
            result = &mut work => { drop(permit); result },
            () = tokio::time::sleep(Duration::from_secs(25)) => {
                let phase = self.entries.read().unwrap_or_else(PoisonError::into_inner).get(&id).map_or(ProbePhase::Reconcile, |entry| entry.refresh.phase);
                self.blocked(&id, PlacementFailure::RunnerUnreachable {phase, timed_out: true}, now());
                if phase == ProbePhase::Reconcile { drop(permit); work.await } else { let result = work.await; drop(permit); result }
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
        self.finish_probe(&id);
    }

    fn finish_probe(&self, id: &str) {
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            entry.refresh.active = false;
            entry.refresh.completed = Some(Instant::now());
            entry.refresh.admitted = None;
        }
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    async fn refresh_connected(&self, id: &str) -> Result<(), PlacementFailure> {
        match self.connection(id)? {
            Connection::Local(catalog) => {
                self.ready_local(catalog, now())?;
            }
            Connection::Remote(client, services) => {
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
                self.ready(id, client, services, options, discovery, now())?;
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
